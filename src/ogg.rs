//! Decoding of Ogg-encapsulated Opus streams.
#![cfg(feature = "ogg")]

use crate::{
    channel::ChannelMapping,
    error::Result,
    multipacket::Decoder,
    sample::{Sample, Samples},
    slice_ext::SliceExt,
};
use ogg::PacketReader;
use std::{
    error,
    fmt::{self, Debug, Display, Formatter},
    io::prelude::*,
    num::NonZeroU32,
};

/// The error type returned when the Ogg Opus stream is malformed.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Clone, Copy, Hash)]
pub enum OggOpusError {
    /// Stream rejected due to a suspected denial-of-service attack.
    DenialOfService,
    /// The Ogg Header packets are incorrectly page-aligned.
    BadPaging,
    /// Either of the Identification Header or the Comment Header had the wrong magic number.
    BadMagic,
    /// The Identificaion Header indicated that this Ogg file conforms to an unsupported version of
    /// the specification.
    UnsupportedVersion,
}

impl Display for OggOpusError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            OggOpusError::DenialOfService => "suspected denial-of-service attack",
            OggOpusError::BadPaging => "bad ogg paging alignment",
            OggOpusError::BadMagic => "invalid magic number",
            OggOpusError::UnsupportedVersion => "unsupported encapsulation specification version",
        })
    }
}

impl error::Error for OggOpusError {}

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
struct IdHeader {
    /// Encapsulation specification version.
    version: u8,
    /// Output channel configuration.
    channels: ChannelMapping,
    /// Number of samples (at 48 kHz) to discard when beginning playback.
    pre_skip: u16,
    /// Sample rate of the original input (before encoding) in Hz.
    ///
    /// This is _not_ the sample rate to use for playback.
    sample_rate: Option<NonZeroU32>,
    /// 20*log_10 of the factor by which to scale the decoder output to
    /// receive the desired playback volume.
    output_gain: i16,
}

impl IdHeader {
    /// Human-Readable codec identification.
    const MAGIC: [u8; 8] = *b"OpusHead";

    /// Major (incompatible) version subfield mask.
    const VERSION_MAJOR_MASK: u8 = 0b1111_0000;

    /// Minor (compatible) version subfield mask.
    const VERSION_MINOR_MASK: u8 = 0b0000_1111;

    /// Create a new ID header representation from bytes.
    fn new(data: &[u8]) -> Result<Self> {
        use byteorder::{ByteOrder, LE};

        if data.get_res(..8)? == Self::MAGIC {
            let version = *data.get_res(8)?;

            if version & IdHeader::VERSION_MAJOR_MASK == 0 {
                Ok(IdHeader {
                    version,
                    channels: ChannelMapping::new(
                        *data.get_res(9)?,
                        *data.get_res(18)?,
                        data.get_res(19..)?,
                    )?,
                    pre_skip: LE::read_u16(data.get_res(10..=11)?),
                    sample_rate: NonZeroU32::new(LE::read_u32(data.get_res(12..=15)?)),
                    output_gain: LE::read_i16(data.get_res(15..=16)?),
                })
            } else {
                Err(OggOpusError::UnsupportedVersion.into())
            }
        } else {
            Err(OggOpusError::BadMagic.into())
        }
    }

    /// Returns the encapsulation specification version as (major, minor).
    fn version(&self) -> (u8, u8) {
        const MAJOR_SHIFT_RIGHT: u32 = IdHeader::VERSION_MAJOR_MASK.trailing_zeros();
        (
            (self.version & IdHeader::VERSION_MAJOR_MASK) >> MAJOR_SHIFT_RIGHT,
            self.version & IdHeader::VERSION_MINOR_MASK,
        )
    }

    /// Returns the output channel configuration.
    fn channels(&self) -> &ChannelMapping {
        &self.channels
    }

    /// Returns the number of samples (at 48 kHz) to discard when beginning playback.
    fn pre_skip(&self) -> u16 {
        self.pre_skip
    }

    /// Returns the encoding sample rate.
    fn sample_rate(&self) -> Option<NonZeroU32> {
        self.sample_rate
    }

    /// Returns 20*log_10 of the factor by which to scale the decoder output to
    /// receive the desired playback volume.
    fn output_gain(&self) -> i16 {
        self.output_gain
    }
}

/// An iterator over user comments.
#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub struct Comments<'a> {
    comments: &'a [u8],
    comments_num: u32,
    comments_read: u32,
    pos: usize,
}

impl<'a> Iterator for Comments<'a> {
    type Item = (&'a str, &'a str);

    fn next(&mut self) -> Option<Self::Item> {
        use byteorder::{ByteOrder, LE};
        use std::str::from_utf8;

        if self.pos < self.comments.len() && self.comments_read < self.comments_num {
            // get comment length
            let cmt_start = self.pos + 4;
            let cmt_len = LE::read_u32(self.comments.get(self.pos..cmt_start)?) as usize;

            // bookkeeping
            // this is located here so that on comment parse failure, calling .next() again returns
            // the next comment
            self.pos = cmt_start + cmt_len;
            self.comments_read += 1;

            // parse comment
            let cmt = from_utf8(self.comments.get(cmt_start..self.pos)?).ok()?;
            let (name, value) = cmt.split_at(cmt.find('=')?);

            Some((name, &value[1..]))
        } else {
            None
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        // this has a lower-bound of zero as the next comment might be malformed
        (0, Some((self.comments_num - self.comments_read) as usize))
    }
}

#[derive(PartialEq, Eq, Clone, Hash)]
struct CommentHeader {
    comments: Box<[u8]>,
    comments_num: u32,
    vendor: String,
}

impl CommentHeader {
    /// Human-Readable codec identification.
    const MAGIC: [u8; 8] = *b"OpusTags";

    /// Maximum length of the packet.
    const PACKET_LEN_MAX: usize = 125_829_120;

    /// Packet position after which to ignore comments.
    const COMMENTS_IGNORE_LEN: usize = 61_440;

    /// Create a new comment header representation from bytes.
    fn new(data: &[u8]) -> Result<Self> {
        use byteorder::{ByteOrder, LE};

        // Denial-of-Service check
        if data.len() > Self::PACKET_LEN_MAX {
            Err(OggOpusError::DenialOfService.into())
        } else if data.get_res(..8)? == Self::MAGIC {
            // only parses the vendor string (for debugging) at initialization
            let comments_start = 12 + LE::read_u32(data.get_res(8..12)?) as usize;
            let vendor = String::from_utf8_lossy(data.get_res(12..comments_start)?).into_owned();
            let num_comments = LE::read_u32(data.get_res(comments_start..comments_start + 4)?);

            // we still save the comment data so that we can parse it later if necessary.
            // also, some more DOS checks
            let comments = if data.len() <= Self::COMMENTS_IGNORE_LEN {
                &data[comments_start + 4..]
            } else {
                &data[comments_start + 4..Self::COMMENTS_IGNORE_LEN]
            }
            .to_owned()
            .into_boxed_slice();

            Ok(CommentHeader {
                comments,
                comments_num: num_comments,
                vendor,
            })
        } else {
            Err(OggOpusError::BadMagic.into())
        }
    }

    /// Returns an iterator over the user comments.
    fn comments(&self) -> Comments<'_> {
        Comments {
            comments: &self.comments[..],
            comments_num: self.comments_num,
            comments_read: 0,
            pos: 0,
        }
    }

    /// Returns the vendor string.
    fn vendor(&self) -> &str {
        &self.vendor[..]
    }
}

impl Debug for CommentHeader {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        let mut ds = f.debug_struct("CommentHeader");

        ds.field("vendor", &self.vendor());
        for (name, value) in self.comments() {
            ds.field(name, &value);
        }

        ds.finish()
    }
}

/// A reader for Ogg Opus files and/or streams.
pub struct OggOpusReader<R: Read + Seek> {
    comments: CommentHeader,
    decoder: Decoder,
    id_header: IdHeader,
    reader: PacketReader<R>,
}

impl<R> OggOpusReader<R>
where
    R: Read + Seek,
{
    /// Creates a new `OggOpusReader` from the given reader.
    pub fn new(reader: R) -> Result<Self> {
        // temporary sample rate to decode at until better infrastructure is installed
        const SAMPLE_RATE_TEMPORARY: u32 = 48_000;

        let mut reader = PacketReader::new(reader);

        // read id header
        let id_packet = reader.read_packet_expected()?;
        let id_header =
            if id_packet.first_in_stream() && id_packet.first_in_page() && id_packet.last_in_page()
            {
                IdHeader::new(&id_packet.data[..])?
            } else {
                return Err(OggOpusError::BadPaging.into());
            };

        // read comment header
        let comments_packet = reader.read_packet_expected()?;
        let comments = if id_packet.first_in_page() && id_packet.last_in_page() {
            CommentHeader::new(&comments_packet.data[..])?
        } else {
            return Err(OggOpusError::BadPaging.into());
        };

        // initialize decoder
        let channels = id_header.channels().mapping_table().streams();
        let decoder = Decoder::new(SAMPLE_RATE_TEMPORARY, channels);

        Ok(OggOpusReader {
            comments,
            decoder,
            id_header,
            reader,
        })
    }

    /// Returns an iterator over user comments contained in the Vorbis comments block.
    #[inline]
    pub fn comments(&self) -> Comments<'_> {
        self.comments.comments()
    }

    /// Returns the number of samples (at 48 kHz) to discard when beginning playback.
    #[inline]
    pub fn pre_skip(&self) -> u16 {
        self.id_header.pre_skip()
    }

    /// Returns the sample rate of the media this file was encoded from, in Hz.
    ///
    /// Note that this is not necessarily the sample rate it will be played back at.
    #[inline]
    pub fn sample_rate(&self) -> Option<NonZeroU32> {
        self.id_header.sample_rate()
    }

    /// Returns 20&thinsp;log<sub>10</sub> of the factor by which to scale the decoder output to
    /// receive the desired playback volume.
    #[inline]
    pub fn output_gain(&self) -> i16 {
        self.id_header.output_gain()
    }

    /// Returns the encoder vendor string from the Vorbis comment block.
    #[inline]
    pub fn vendor(&self) -> &str {
        self.comments.vendor()
    }

    /// Returns the encapsulation specification version as (major, minor).
    #[inline]
    pub fn version(&self) -> (u8, u8) {
        self.id_header.version()
    }

    /// A lower-level interface, decoding the next multipacket on each call.
    ///
    /// Returns either an error, or the number of samples read per channel into `buf`.
    pub fn read_samples<S, T>(&mut self, buf: &mut S) -> Result<usize>
    where
        S: Samples<T>,
        T: Sample,
    {
        use crate::multipacket::Multipacket;

        let ogg_packet = self.reader.read_packet()?;

        if let Some(ogg_packet) = ogg_packet {
            let mapping_table = self.id_header.channels().mapping_table();
            let multipacket = Multipacket::new(&ogg_packet.data[..], mapping_table)?;
            Ok(self.decoder.decode(Some(multipacket), buf)?)
        } else {
            Ok(0)
        }
    }

    /// Returns the wrapped reader, consuming the `OggOpusReader`.
    #[inline]
    pub fn into_inner(self) -> R {
        self.reader.into_inner()
    }
}

impl<R> Debug for OggOpusReader<R>
where
    R: Read + Seek,
{
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        #[derive(PartialEq, Eq, Clone, Copy, Hash)]
        struct ElidedStruct<'a>(&'a str);
        impl Debug for ElidedStruct<'_> {
            fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
                f.pad(self.0)
            }
        }

        f.debug_struct("OggOpusReader")
            .field("reader", &ElidedStruct("PacketReader"))
            .field("id_header", &self.id_header)
            .field("comments", &self.comments)
            .finish()
    }
}
