//! Support for decoding of RTP streams using
//! [SMPTE 2022-1](https://en.wikipedia.org/wiki/SMPTE_2022) Forward Error Correction, also know
//! as 'Pro-MPEG Code of Practice #3' or '1D/2D parity FEC' or '2dparityfec'.

#![forbid(unsafe_code)]
#![deny(rust_2018_idioms, future_incompatible)]

pub mod heap_pool;

use rtp_rs::IntoSeqIterator;
use rtp_rs::RtpReader;
use rtp_rs::Seq;
use smpte2022_1_packet as fec;
use smpte2022_1_packet::FecHeader;
use std::collections::VecDeque;
use std::marker;

pub trait Receiver<P: PacketRef> {
    fn receive(&mut self, packets: impl Iterator<Item = P>);
}

#[derive(Debug)]
enum FecGeometryError {
    /// We can't work out the FEC settings given a 'row' packet, we need the headers from a
    /// 'column' packet
    ColumnPacketRequired,
    BadNumberOfRows(u8),
    BadNumberOfColumns(u8),
    BadMatrixSize(u16),
}

struct FecGeometry {
    /// Number of columns
    l: u8,
    /// Number of rows
    d: u8,
    /// Type of error correction (will always be 0)
    fec_type: u8,
}
impl FecGeometry {
    /// The minimum number of allowed rows or columns, according to the spec
    const MIN_LENGTH: u8 = 4;

    /// The maximum number of allowed rows or columns, according to the spec
    const MAX_LENGTH: u8 = 20;

    /// The maximum overall size of the FEC matrix (rows * columns)
    const MAX_AREA: u16 = 100;

    fn size_of(header: &fec::FecHeader<'_>) -> u16 {
        (header.number_associated() as u16) * (header.offset() as u16)
    }

    fn from_header(header: &fec::FecHeader<'_>) -> Result<FecGeometry, FecGeometryError> {
        if header.orientation() == fec::Orientation::Row {
            Err(FecGeometryError::ColumnPacketRequired)
        } else if header.offset() > Self::MAX_LENGTH || header.offset() < Self::MIN_LENGTH {
            Err(FecGeometryError::BadNumberOfColumns(header.offset()))
        } else if header.number_associated() > Self::MAX_LENGTH
            || header.number_associated() < Self::MIN_LENGTH
        {
            Err(FecGeometryError::BadNumberOfRows(
                header.number_associated(),
            ))
        } else if Self::size_of(header) > Self::MAX_AREA {
            Err(FecGeometryError::BadMatrixSize(Self::size_of(header)))
        } else {
            Ok(FecGeometry {
                l: header.offset(),
                d: header.number_associated(),
                fec_type: header.fec_type(),
            })
        }
    }

    fn matches(&self, header: &fec::FecHeader<'_>) -> bool {
        let oriented_ok = match header.orientation() {
            fec::Orientation::Column => {
                self.d == header.number_associated() && self.l == header.offset()
            }
            fec::Orientation::Row => self.l == header.number_associated(),
        };
        oriented_ok && self.fec_type == header.fec_type()
    }
}

pub trait Packet: Sized {
    type R: PacketRef<P = Self>;
    type W: PacketRefMut<P = Self>;

    fn into_ref(self) -> Self::R;
    fn into_ref_mut(self) -> Self::W;
}
pub trait PacketRef: Sized {
    type P: Packet;

    fn payload(&self) -> &[u8];

    /// Attempt to re-obtain the Packet to which this is a reference, returning `Result::Err`
    /// containing the `HeapPacketRef` if other references to the packet still exist.
    fn try_into_packet(self) -> Result<Self::P, Self>;
}
pub trait PacketRefMut {
    type P: Packet;

    fn payload(&mut self) -> &mut [u8];

    fn into_packet(self) -> Self::P;

    /// adjust the size of the underlying buffer to the given value
    ///
    /// ##Panics
    ///
    /// Will panic if the given size is larger than the packet size of the `BufferPool` which
    /// allocated this packet.
    fn truncate(&mut self, size: usize);
}

pub trait BufferPool {
    type P: Packet;

    fn allocate(&self) -> Option<Self::P>;
}

struct NilReceiver<P: PacketRef> {
    phantom: marker::PhantomData<P>,
}
impl<P: PacketRef> Receiver<P> for NilReceiver<P> {
    fn receive(&mut self, packets: impl Iterator<Item = P>) {}
}

struct SeqEntry<P: PacketRef> {
    // TODO: we could actually just store the base SN value in PacketSequence and work out the 'seq'
    //       value here from that.  For now keep it explicit to enable more assertions while I'm
    //       still working out the implementation.
    seq: rtp_rs::Seq,
    pk: Option<P>,
}
struct PacketSequence<P: PacketRef, Recv: Receiver<P>> {
    size_limit: usize,
    packets: VecDeque<SeqEntry<P>>,
    recv: Recv,
}
impl<P: PacketRef> PacketSequence<P, NilReceiver<P>> {
    pub fn new(size_limit: usize) -> PacketSequence<P, NilReceiver<P>> {
        PacketSequence {
            size_limit,
            packets: VecDeque::with_capacity(size_limit),
            recv: NilReceiver {
                phantom: marker::PhantomData,
            },
        }
    }
}
impl<P: PacketRef, Recv: Receiver<P>> PacketSequence<P, Recv> {
    pub fn new_with_receiver(size_limit: usize, recv: Recv) -> PacketSequence<P, Recv> {
        PacketSequence {
            size_limit,
            packets: VecDeque::with_capacity(size_limit),
            recv,
        }
    }

    pub fn insert(&mut self, seq: rtp_rs::Seq, pk: P) {
        if let Some(base_seq) = self.front_seq() {
            self.remove_outdated(seq, base_seq);
        }
        if let Some(last_seq) = self.back_seq() {
            // fill any gaps in the sequence with placeholders,
            let expected = last_seq.next();
            if expected < seq {
                for s in (expected..seq).seq_iter() {
                    self.packets.push_back(SeqEntry { seq: s, pk: None });
                }
            }
            if last_seq < seq {
                self.packets.push_back(SeqEntry { seq, pk: Some(pk) });
            } else {
                self.packets
                    .iter_mut()
                    .find(|p| p.seq == seq)
                    .map(|p| p.pk = Some(pk));
            }
        } else {
            self.packets.push_back(SeqEntry { seq, pk: Some(pk) });
        }
        self.check();
    }

    fn check(&self) {
        assert!(
            self.packets.len() <= self.size_limit,
            "packets.len={} should-be-less-or-equal-size_limit={}",
            self.packets.len(),
            self.size_limit
        );
        let mut last: Option<Seq> = None;
        for (i, p) in self.packets.iter().enumerate() {
            if let Some(seq) = last {
                assert_eq!(p.seq, seq.next(), "at index {}", i);
            }
            last = Some(p.seq);
        }
    }

    fn front_seq(&self) -> Option<Seq> {
        self.packets.front().map(|p| p.seq)
    }

    fn back_seq(&self) -> Option<Seq> {
        self.packets.back().map(|p| p.seq)
    }

    fn get_by_seq(&self, seq: Seq) -> Option<&P> {
        // TODO: we can locate the item directly without searching
        self.packets
            .iter()
            .find(|p| p.seq == seq)
            .and_then(|p| p.pk.as_ref())
    }

    fn remove_outdated(&mut self, seq_new: Seq, seq_base: Seq) {
        let seq_delta = seq_new - seq_base;
        if seq_delta > 0 && seq_delta as usize >= self.size_limit {
            let to_remove = seq_delta as usize - self.size_limit;
            let drain = if to_remove >= self.packets.len() {
                println!(
                    "Large jump {} in seq from start={:?} to latest={:?}",
                    seq_delta, seq_base, seq_new
                );
                self.packets.drain(..)
            } else {
                self.packets.drain(0..=to_remove)
            };
            self.recv.receive(drain.filter_map(|e| e.pk));
        }
    }
}

/// Tracks the current state of the Forward Error Correction process, holding references to a
/// rolling window of packets already acquired, and using the opportunity of new packets becoming
/// available to reconstruct any missing packets within the rolling window.
///
/// ```
///  main packet descriptors
///  |
///  v            v--- row packet descriptors
///  P  P  P  P | R
///             |
///  P  P  P  P | R
///             |
///  P  P  P  P | R
///             |
///  P  P  P  P | R
///  -----------+
///  C  C  C  C     <-- column FEC descriptors
/// ```
struct FecMatrix<BP: BufferPool, Recv: Receiver<<BP::P as Packet>::R>> {
    buffer_pool: BP,
    main_descriptors: PacketSequence<<BP::P as Packet>::R, Recv>,
    row_descriptors: PacketSequence<<BP::P as Packet>::R, NilReceiver<<BP::P as Packet>::R>>,
    col_descriptors: PacketSequence<<BP::P as Packet>::R, NilReceiver<<BP::P as Packet>::R>>,
}
impl<BP: BufferPool, Recv: Receiver<<BP::P as Packet>::R>> FecMatrix<BP, Recv> {
    /// Panics if there would be more than 100 entries in the matrix
    pub fn new(buffer_pool: BP, cols: u8, rows: u8, receiver: Recv) -> FecMatrix<BP, Recv> {
        assert!(cols * rows <= 100);
        let matrix_size = cols as usize * rows as usize * 2;
        FecMatrix {
            buffer_pool,
            main_descriptors: PacketSequence::new_with_receiver(matrix_size, receiver),
            row_descriptors: PacketSequence::new(rows as usize),
            col_descriptors: PacketSequence::new(cols as usize),
        }
    }

    // TODO: Explicitly report to calling code the SN values for packets definitely lost.
    //       Support calling code collecting stats about number of losses + corrections.

    pub fn insert(&mut self, seq: rtp_rs::Seq, pk: BP::P) -> Result<Option<BP::P>, FecDecodeError> {
        let pk_ref = pk.into_ref();
        self.main_descriptors.insert(seq, pk_ref);

        // if we already have FEC packets covering this media packet (because things arrived out
        // of sequence) then the arrival of this packet may now make it possible to apply a
        // correction, so search for those opportunities in the row + column to which the packet
        // belongs,
        self.look_for_col_correction(seq);
        self.look_for_row_correction(seq);
        // TODO: return any regenerated packets!  Will need a way to produce more than one
        Ok(None)
    }

    fn look_for_col_correction(&mut self, seq: rtp_rs::Seq) {
        //unimplemented!()
    }

    fn look_for_row_correction(&mut self, seq: rtp_rs::Seq) {
        //unimplemented!()
    }

    pub fn insert_column(
        &mut self,
        seq: rtp_rs::Seq,
        pk: BP::P,
    ) -> Result<Option<BP::P>, FecDecodeError> {
        let pk_ref = pk.into_ref();
        let rtp_header = rtp_rs::RtpReader::new(pk_ref.payload())?;
        let (fec_header, fec_payload) = fec::FecHeader::split_from_bytes(rtp_header.payload())?;

        let res = self.maybe_correct(fec_header, fec_payload);

        self.col_descriptors.insert(seq, pk_ref);
        Ok(res)
    }

    pub fn insert_row(
        &mut self,
        seq: rtp_rs::Seq,
        pk: BP::P,
    ) -> Result<Option<BP::P>, FecDecodeError> {
        let pk_ref = pk.into_ref();
        let rtp_header = rtp_rs::RtpReader::new(pk_ref.payload())?;
        let (fec_header, fec_payload) = fec::FecHeader::split_from_bytes(rtp_header.payload())?;

        let res = self.maybe_correct(fec_header, fec_payload);

        self.row_descriptors.insert(seq, pk_ref);
        Ok(res)
    }

    /// returns an iterator over the main packets associated with the given FEC packet
    fn iter_associated(
        &self,
        fec_header: &FecHeader<'_>,
    ) -> impl Iterator<Item = (Seq, Option<&<BP::P as Packet>::R>)> {
        let sn_start = ((fec_header.sn_base() & 0xffff) as u16).into();
        let sn_end = sn_start + fec_header.number_associated() as u16 * fec_header.offset() as u16;

        (sn_start..sn_end)
            .seq_iter()
            .step_by(fec_header.offset() as usize)
            .map(move |seq| (seq, self.main_descriptors.get_by_seq(seq)))
    }

    fn find_single_missing_associated(&self, fec_header: &FecHeader<'_>) -> Option<Seq> {
        let mut missing_seq = None;
        for (seq, pk) in self.iter_associated(&fec_header) {
            if pk.is_none() {
                match missing_seq {
                    Some(_) => return None, // can't recover if more than 1 missing
                    None => missing_seq = Some(seq),
                }
            }
        }
        missing_seq
    }

    fn maybe_correct<'a>(
        &mut self,
        fec_header: FecHeader<'a>,
        fec_payload: &'a [u8],
    ) -> Option<BP::P> {
        let missing_seq = self.find_single_missing_associated(&fec_header);
        if let Some(seq) = missing_seq {
            let recovered = self.buffer_pool.allocate();
            if recovered.is_none() {
                eprintln!("failed to allocate buffer from pool");
                return None;
            }
            let mut recovered_ref = recovered.unwrap().into_ref_mut();
            recovered_ref.truncate(fec_payload.len() + 12);
            let payload = recovered_ref.payload();
            // the 'payload' of the FEC packet protects the payload of the media packets, but
            // not the headers, also prompeg disallows CSRC
            payload[12..].copy_from_slice(fec_payload);
            let mut len_recover = fec_header.length_recovery();
            let mut ts_recover = fec_header.ts_recovery() + 12;
            for (_, pk) in self.iter_associated(&fec_header) {
                if let Some(pk) = pk {
                    Self::xor(payload, pk.payload());
                    len_recover ^= pk.payload().len() as u16;
                    let rtp = RtpReader::new(pk.payload()).unwrap();
                    ts_recover ^= rtp.timestamp();
                }
            }
            let mut rtp = RtpHeaderMut::new(payload);
            rtp.set_timestamp(ts_recover);
            rtp.set_sequence(seq);
            // TODO: report the recovery to the 'Receiver' instance
            if RtpReader::new(payload).unwrap().sequence_number() != seq {
                println!(
                    "{:?} Just recovered {:?}, but was aiming for {:?}! (recovered ts is {})",
                    fec_header.orientation(),
                    RtpReader::new(payload).unwrap().sequence_number(),
                    seq,
                    RtpReader::new(payload).unwrap().timestamp(),
                );
            }
            recovered_ref.truncate(len_recover as usize);
            Some(recovered_ref.into_packet())
        } else {
            None
        }
    }

    /// Performs exclusive-or of every byte in `src` with the corresponding byte in `dst`, placing
    /// the result in `dst`.
    ///
    /// Panics if `src` and `dst` do not have the same length
    fn xor(dst: &mut [u8], src: &[u8]) {
        assert_eq!(dst.len(), src.len());
        for (d, s) in dst.iter_mut().zip(src.iter()) {
            *d ^= s
        }
    }
}

struct RtpHeaderMut<'buf>(&'buf mut [u8]);
impl RtpHeaderMut<'_> {
    fn new(buf: &mut [u8]) -> RtpHeaderMut<'_> {
        assert!(buf.len() >= RtpReader::MIN_HEADER_LEN);
        RtpHeaderMut(buf)
    }
    fn set_sequence(&mut self, seq: Seq) {
        let s: u16 = seq.into();
        self.0[2] = (s >> 8) as u8;
        self.0[3] = (s & 0xff) as u8;
    }
    fn set_timestamp(&mut self, ts: u32) {
        self.0[4] = (ts >> 24) as u8;
        self.0[5] = (ts >> 16 & 0xff) as u8;
        self.0[6] = (ts >> 8 & 0xff) as u8;
        self.0[7] = (ts & 0xff) as u8;
    }
}

#[derive(Debug)]
pub enum FecDecodeError {
    Rtp(rtp_rs::RtpHeaderError),
    Fec(fec::FecHeaderError),
    Orientation {
        actual: fec::Orientation,
        expected: fec::Orientation,
    },
}
impl From<rtp_rs::RtpHeaderError> for FecDecodeError {
    fn from(v: rtp_rs::RtpHeaderError) -> Self {
        FecDecodeError::Rtp(v)
    }
}
impl From<fec::FecHeaderError> for FecDecodeError {
    fn from(v: fec::FecHeaderError) -> Self {
        FecDecodeError::Fec(v)
    }
}

// TODO:
//  - Should we provide means for the application to 'flush' packets held in the FEC matrix which
//    aren't getting delivered to the application because the input is stopped / paused?

enum State<BP: BufferPool, Recv: Receiver<<BP::P as Packet>::R>> {
    /// This state just exits so that overwrite some other state during the transition from one
    /// state to another.
    Init,
    Start(BP, Recv),
    Running {
        geometry: FecGeometry,
        matrix: FecMatrix<BP, Recv>,
    },
}
impl<BP: BufferPool, Recv: Receiver<<BP::P as Packet>::R>> State<BP, Recv> {
    fn running(&mut self, width: u8, height: u8, geometry: FecGeometry) {
        *self = match std::mem::replace(self, State::Init) {
            State::Start(buffer_pool, receiver) => State::Running {
                geometry,
                matrix: FecMatrix::new(buffer_pool, width, height, receiver),
            },
            _ => panic!("Only State::Start is supported by to_running()"),
        }
    }

    fn insert_main_packet(
        &mut self,
        seq: rtp_rs::Seq,
        pk: BP::P,
    ) -> Result<Option<BP::P>, FecDecodeError> {
        // if not Running, there's nothing to do; calling code should forward packet on to receiver
        if let State::Running { ref mut matrix, .. } = self {
            Ok(matrix.insert(seq, pk)?)
        } else {
            Ok(None)
        }
    }

    fn insert_column_packet(
        &mut self,
        seq: rtp_rs::Seq,
        pk: BP::P,
    ) -> Result<Option<BP::P>, FecDecodeError> {
        // if not Running, there's nothing to do
        if let State::Running { ref mut matrix, .. } = self {
            Ok(matrix.insert_column(seq, pk)?)
        } else {
            Ok(None)
        }
    }

    fn insert_row_packet(
        &mut self,
        seq: rtp_rs::Seq,
        pk: BP::P,
    ) -> Result<Option<BP::P>, FecDecodeError> {
        // if not Running, there's nothing to do
        if let State::Running { ref mut matrix, .. } = self {
            Ok(matrix.insert_row(seq, pk)?)
        } else {
            Ok(None)
        }
    }
}

/// Decoder state-machine for _SMPTE 2022-1_ FEC.
///
/// The decoder instance owns the storage for all RTP packets being processed.  An application
/// receiving data from the network will borrow buffers from the decoder and arrange for UDP
/// packet payloads to be written into these.
///
/// Note that this does not try to solve the following problems,
///  - Reordering
///    - Out-of-order packets on the main stream are passed on to the application still out-of-order
///    - Recovery of lost packets may occur after packets with later sequence numbers have already
///      been passed to the application
///  - Pacing
///    - Packets are passed to the application as soon as possible, without regard for the
///      timestamps on the packets
///
/// ## Planned changes
///
/// This design will change in future as required to
/// support things like `AF_XDP`, and the caller will be able to plug in their own allocation
/// strategy.  In this future design, the decoder will just hold on to references to packets
pub struct Decoder<BP: BufferPool, Recv: Receiver<<BP::P as Packet>::R>> {
    state: State<BP, Recv>,
}
impl<BP: BufferPool, Recv: Receiver<<BP::P as Packet>::R>> Decoder<BP, Recv> {
    ///  - `max_packet_len` A common size limit for UDP payloads is 1,472 bytes, but you might want
    ///    to supply a larger limit if it's possible that jumbo frames might me used by the network
    ///    and sending application.
    ///  - `max_packet_batch_size` The largest number of packets that the caller wants to be able
    ///    to pass in one batch.  Controls the sizes of the lists returned by `next_*_buffers()`
    ///    methods.
    pub fn new(buffer_pool: BP, receiver: Recv) -> Decoder<BP, Recv> {
        Decoder {
            state: State::Start(buffer_pool, receiver),
        }
    }

    pub fn add_main_packets<T: Iterator<Item = BP::P>>(
        &mut self,
        pk: T,
    ) -> Result<(), FecDecodeError> {
        for p in pk {
            let pk_ref = p.into_ref();
            let rtp_header = rtp_rs::RtpReader::new(pk_ref.payload())?;
            // TODO: check that:
            //       - CSRC is not used
            //       - extension usage is unchanging
            self.state.insert_main_packet(
                rtp_header.sequence_number(),
                pk_ref.try_into_packet().unwrap_or_else(|e| unreachable!()),
            )?;
        }
        Ok(())
    }

    pub fn add_row_packets<T: Iterator<Item = BP::P>>(
        &mut self,
        pk: T,
    ) -> Result<(), FecDecodeError> {
        for p in pk {
            let pk_ref = p.into_ref();
            let rtp_header = rtp_rs::RtpReader::new(pk_ref.payload())?;
            let fec_header = fec::FecHeader::from_bytes(rtp_header.payload())?;
            if fec_header.orientation() != fec::Orientation::Row {
                return Err(FecDecodeError::Orientation {
                    expected: fec::Orientation::Row,
                    actual: fec::Orientation::Column,
                });
            }
            self.merge_fec_parameters(fec_header);
            let mut recovered = self.state.insert_row_packet(
                rtp_header.sequence_number(),
                pk_ref.try_into_packet().unwrap_or_else(|e| unreachable!()),
            )?;
            while let Some(pk) = recovered {
                let pk_ref = pk.into_ref();
                let rtp_header = rtp_rs::RtpReader::new(pk_ref.payload())?;
                let seq = rtp_header.sequence_number();
                recovered = self.state.insert_main_packet(
                    seq,
                    pk_ref.try_into_packet().unwrap_or_else(|e| unreachable!()),
                )?;
            }
        }
        Ok(())
    }

    pub fn add_column_packets<T: Iterator<Item = BP::P>>(
        &mut self,
        pk: T,
    ) -> Result<(), FecDecodeError> {
        for p in pk {
            let pk_ref = p.into_ref();
            let rtp_header = rtp_rs::RtpReader::new(pk_ref.payload())?;
            let fec_header = fec::FecHeader::from_bytes(rtp_header.payload())?;
            if fec_header.orientation() != fec::Orientation::Column {
                return Err(FecDecodeError::Orientation {
                    expected: fec::Orientation::Column,
                    actual: fec::Orientation::Row,
                });
            }
            self.merge_fec_parameters(fec_header);
            self.state.insert_column_packet(
                rtp_header.sequence_number(),
                pk_ref.try_into_packet().unwrap_or_else(|e| unreachable!()),
            )?;
        }
        Ok(())
    }

    fn merge_fec_parameters(&mut self, header: fec::FecHeader<'_>) {
        // TODO: Grace period after FEC parameter change to avoid DOS attack due to having to
        //       reallocate buffers every time a packet arrives in worst case.

        // TODO: Maybe just drop any row-packets that don't match the current state (so we rely on
        //       packets of the first stream arriving to change the config).  This would avoid
        //       the state flip-flopping all the time if the FEC streams have mismatched settings.
        match self.state {
            State::Init => panic!("self.state is State::Init"),
            State::Start(ref buffer_pool, ref receiver) => {
                if let Ok(geometry) = FecGeometry::from_header(&header) {
                    let width = geometry.d;
                    let height = geometry.l;
                    self.state.running(width, height, geometry);
                }
            }
            State::Running {
                ref mut geometry,
                ref matrix,
            } => {
                if !geometry.matches(&header) {
                    let geom = FecGeometry::from_header(&header).unwrap(); // FIXME
                    *geometry = geom;
                    eprintln!("Ooof; needed to reset FEC geom")
                    // TODO: reset any other state
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap_pool::HeapPacketRef;
    use crate::heap_pool::HeapPool;
    use hex_literal::*;
    use std::io::Write;

    struct TestReceiver;
    impl Receiver<HeapPacketRef> for TestReceiver {
        fn receive(&mut self, packets: impl Iterator<Item = HeapPacketRef>) {
            unimplemented!()
        }
    }

    #[test]
    fn it_works() {
        let buffer_pool = HeapPool::new(1, 1500);
        let mut decoder = Decoder::new(buffer_pool.clone(), TestReceiver);

        let row_pk = {
            let row_pk = buffer_pool.allocate().unwrap();
            let data = hex!("80607bc30d5c28f1000000007e6100008000000000000d6640011400001efe0b97249db8582ac143b90e9b3975a18c703817a6b1ef1035dc70edc70b67e407f57b0db156065f95baabaeeaab82d3b6a869b117c327c8f91e3b521c78a63e24bafe955d907258815b273fcc14a7d05ad3844139bb41bab400654e3e94fa3609234532e126c18a525154f0fcc52347e2c1a842d667de613fc0c79b475daffd50e0a4270df99310ed69ef211a1f2c7cdb0cda7dace687a34fb125ca480f3636764c01c42ed3bacb2e8d5eecbf11332056d0e670f380d77abbb3001eff0e5e42da393c535a3025e40cab385fd1059cc67337f9871b8823742999391d72404e65db85426ecfd34ea30e8d4850440903a7fe4c6dc0f6494153e08e2d880cd54d8f5e76afeaaee17257af357fe28404f788851369dc93b384c437620664692a9a835d2a54b191d18354e433b5955e41a15621acf0ee0b8813c8db09e4148f6c8069c4618319169c663f5d7907c15855c491c340f6ce1708e7afb99d778fd18714caba17411734f69069134964aa7210041a4a7f9619dac8001efd2740d92f5846c3222be733cdb938489ca0d0a927e4f8dbbc116da9999581b9fc757427c2d6aa7c967f10972e147b51928cd9b375c12eaac8c43740ee8e3b9da85183a9b61e23e969f6e00fac6e92d5c821c4a29cf0500608c6b539c96266c188b9f2b1f3a04f827b763a75cf6b5c659b63afcb58000b330b12325a1df557c0f636d3c2504b46327713da56f68ea5e233c634a3f566800b0d38f2ff9b5a1ba0712f45f43209a4742608fc68ede9c33ff6a37b1531c07dd66cc8005efd0de4fdea3f01961dbb0d5f138a06d241c582942ef929fd59bb0dfb4d0a15c04c0c4a2623056a7ba8000663f314fc9bc4a60c46fb314d665e074aa6124bde35d23393f951e1ebf02429f5ccce4da6b312fb31840b11f54d825f77504dcd4d4a0fbfa343c110baa3dda2f5ca1e0a3d31fda1edda34d946009c78a86780f8be8c9cb4423a6d2c452cbd9c9ddaf567836c2154cdbe7f781a4a5e6e042d8715a514962c38436885bfd632b728c8404ea2b56ecf2f095d4d98b84ee50000012e0739f29e76c8c6efd1603ebe9aa00179624fe383a56b562fb893d4ec9bad6eae1d0206ba6557543530e109a866afbaf50c9671ccb3daaf0f62fe44ae30ee33fc72dac60bd3f9f7c2d6638ebba1a8a1528acff6f278902caa09060e84a3fb64cd403de93318d0a8724b1bb266ebf6fc456b2f643b8a81f7b898599485349c40eae783217f030a37c7f01c100260932514c8060fe41437364d1aedf8b554077df6de7f8d147040ead501d28e613273b51b1e632932b92b5a860000000ac26f8e44befa5b6b1b1cd5a60da71a8a534fa3f34fb580c9634d3609493318efb796df9b61ae1437363dc74e01896c7eca291d9d1cac3db85a57c041924f55ff74e756570000000c4941f7a9ce78b76e8a7b6a400a26b96c482bdd077f4f1f4433e3ca4faee62317f2a0b663d6be0f8cbce3e292470382d738d50a07de61188b217f4d57b6ea7b830f7154bfd60e3a74c0f2030b1873afa27ceffa597744a583831a563852647cf1051b8967d6e6f41d552637512c69fd0b001eff0f7f663d7cf5a87ab377056df8517b89a45abee3553a8ff4c3c448ddff1e4cb32d401d5ee2b95e3615f050e7e4eeda4e76ca667840e3acb10a9b627a4b251fa28d23c7c40dc486c50e878914ea844ccc55069d948543dd01d5640db169090fd5ea411dfa796f818438b93ae53f2f3e7b15413868044fd717d4c4c8356023d470898d5d72ae72ba2a8887f8af4ab4dd5167126f47d98b91284108e64e886073ad31173abd2f06f762e29a2bd8ce34a323e7cc9027c7faeecd9d");
            let mut ref_mut = row_pk.into_ref_mut();
            ref_mut.payload().write(&data[..]).unwrap();
            ref_mut.into_packet()
        };
        decoder.add_row_packets(vec![row_pk].into_iter()).unwrap();
    }
}
