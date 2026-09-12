//! Hand-authored MPEG-1 syntax: no encoder or external recording is needed.
mod support;

use mpeg2toh264::{
    mpeg2_video_timeline, transcode, Fragment, PictureEncoder, Progress, Session, TranscodeOptions,
};
use std::collections::HashMap;
use support::{adts_stream, mux_transport_stream, split_annex_b, PesUnit};

#[derive(Default)]
struct Bits {
    bytes: Vec<u8>,
    count: usize,
}
impl Bits {
    fn u(&mut self, value: u32, width: usize) {
        for shift in (0..width).rev() {
            if self.count % 8 == 0 {
                self.bytes.push(0);
            }
            let last = self.bytes.len() - 1;
            self.bytes[last] |= (((value >> shift) & 1) as u8) << (7 - self.count % 8);
            self.count += 1;
        }
    }
    fn vlc(&mut self, value: &str) {
        for byte in value.bytes() {
            self.u(u32::from(byte == b'1'), 1);
        }
    }
    fn append(self, stream: &mut Vec<u8>, code: u8) {
        stream.extend_from_slice(&[0, 0, 1, code]);
        stream.extend(self.bytes);
    }
}

fn elementary_stream() -> Vec<u8> {
    let mut stream = Vec::new();
    let mut sequence = Bits::default();
    for (value, width) in [
        (16, 12),
        (16, 12),
        (1, 4),
        (3, 4),
        (1000, 18),
        (1, 1),
        (20, 10),
        (0, 1),
        (0, 1),
        (0, 1),
    ] {
        sequence.u(value, width);
    }
    sequence.append(&mut stream, 0xb3);
    // Decode order I0 P2 B1; each picture has one intra macroblock. The P/B
    // headers deliberately retain MPEG-1's motion fields and no extensions.
    for (kind, temporal_reference) in [(1, 0), (2, 2), (3, 1)] {
        let mut picture = Bits::default();
        picture.u(temporal_reference, 10);
        picture.u(kind, 3);
        picture.u(0xffff, 16);
        if kind >= 2 {
            picture.u(0, 1);
            picture.u(1, 3);
        }
        if kind == 3 {
            picture.u(0, 1);
            picture.u(1, 3);
        }
        picture.u(0, 1);
        picture.append(&mut stream, 0);
        let mut slice = Bits::default();
        slice.u(2, 5); // quantiser_scale_code
        slice.u(0, 1); // extra_bit_slice
        slice.vlc("1"); // macroblock_address_increment = 1
        slice.vlc(if kind == 1 { "1" } else { "00011" }); // intra
        for block in 0..6 {
            slice.vlc(if block < 4 { "100" } else { "00" }); // DC size = 0
            slice.vlc("000001"); // ESCAPE
            slice.u(0, 6); // run = 0
                           // Cover signed eight-bit levels and both extended escape forms.
            match block % 3 {
                0 => slice.u(1, 8),
                1 => {
                    slice.u(0, 8);
                    slice.u(128, 8);
                }
                _ => {
                    slice.u(128, 8);
                    slice.u(127, 8);
                } // -129
            }
            slice.vlc("10"); // end_of_block
        }
        slice.append(&mut stream, 1);
    }
    stream.extend_from_slice(&[0, 0, 1, 0xb7]);
    stream
}

#[test]
fn mpeg1_ipb_escape_pictures_and_timeline_agree() {
    let source = elementary_stream();
    let result = transcode(&source, TranscodeOptions::default()).expect("MPEG-1 converts");
    assert_eq!(result.pictures_converted, 3);
    assert_eq!(result.pictures_skipped, 0);
    assert!(result.undecodable.iter().all(|bad| !bad));
    let timeline = mpeg2_video_timeline(&source, false, &result.undecodable).expect("timeline");
    assert_eq!(timeline.presentation_indices, [1, 3, 2]);
    assert_eq!(timeline.sample_duration, 3600);
    let nals = split_annex_b(&result.bitstream);
    // The initial random-access IDR is an additional reference copy.
    assert_eq!(
        nals.iter()
            .filter(|(kind, _)| *kind == 1 || *kind == 5)
            .count(),
        timeline.presentation_indices.len() + 1
    );
    assert_eq!(nals.iter().filter(|(kind, _)| *kind == 5).count(), 1);
}

fn media(fragments: &[Fragment]) -> Vec<(&[u8], usize, usize)> {
    fragments
        .iter()
        .filter_map(|fragment| match fragment {
            Fragment::Media {
                data,
                video_samples,
                audio_samples,
                ..
            } => Some((data.as_slice(), *video_samples, *audio_samples)),
            _ => None,
        })
        .collect()
}

#[test]
fn mpeg1_ts_with_aac_matches_deferred_session() {
    let source = elementary_stream();
    let audio = adts_stream(6, 3, 2); // 48 kHz, enough audio for three 25 Hz frames.
    let stream = mux_transport_stream(
        &[(0x101, 0x01), (0x102, 0x0f)],
        &[
            PesUnit {
                pid: 0x102,
                stream_id: 0xc0,
                pts: Some(90000),
                payload: &audio,
            },
            PesUnit {
                pid: 0x101,
                stream_id: 0xe0,
                pts: Some(90000),
                payload: &source,
            },
        ],
        &mut HashMap::new(),
    );
    let mut sequential = Session::new(TranscodeOptions::default());
    let mut expected = Vec::new();
    for chunk in stream.chunks(97) {
        expected.extend(sequential.push(chunk).expect("push"));
    }
    expected.extend(sequential.finish().expect("finish"));
    let mut deferred = Session::new(TranscodeOptions::default());
    let mut encoder = PictureEncoder::new();
    let mut actual = Vec::new();
    let mut drive = |mut progress: Progress, session: &mut Session| loop {
        match progress {
            Progress::Idle(fragments) => {
                actual.extend(fragments);
                break;
            }
            Progress::Pending { fragments, jobs } => {
                actual.extend(fragments);
                let outputs: Vec<_> = jobs
                    .iter()
                    .map(|job| encoder.encode(job).expect("encode"))
                    .collect();
                assert!(outputs.iter().all(|output| output.decoded));
                progress = session.complete(&outputs).expect("complete");
            }
        }
    };
    for chunk in stream.chunks(97) {
        let progress = deferred.push_deferred(chunk).expect("push deferred");
        drive(progress, &mut deferred);
    }
    let progress = deferred.finish_deferred().expect("finish deferred");
    drive(progress, &mut deferred);
    assert_eq!(
        media(&expected)
            .iter()
            .map(|(_, video, _)| video)
            .sum::<usize>(),
        4
    );
    assert!(
        media(&expected)
            .iter()
            .map(|(_, _, audio)| audio)
            .sum::<usize>()
            > 0
    );
    assert_eq!(media(&actual), media(&expected));
}

#[test]
fn full_pel_motion_predictors_and_skipped_macroblocks_keep_their_units() {
    use mpeg2toh264::bitreader::BitReader;
    use mpeg2toh264::mpeg2::headers::parse_elementary_stream;
    use mpeg2toh264::mpeg2::macroblock::{decode_slice, MacroblockGrid};

    for kind in [2, 3] {
        for (full_forward, full_backward) in
            [(false, false), (true, false), (false, true), (true, true)]
        {
            let mut source = Vec::new();
            let mut sequence = Bits::default();
            for (value, width) in [
                (64, 12),
                (16, 12),
                (1, 4),
                (3, 4),
                (1000, 18),
                (1, 1),
                (20, 10),
                (0, 1),
                (0, 1),
                (0, 1),
            ] {
                sequence.u(value, width);
            }
            sequence.append(&mut source, 0xb3);
            let mut picture = Bits::default();
            picture.u(0, 10);
            picture.u(kind, 3);
            picture.u(0xffff, 16);
            picture.u(u32::from(full_forward), 1);
            picture.u(1, 3);
            if kind == 3 {
                picture.u(u32::from(full_backward), 1);
                picture.u(1, 3);
            }
            picture.u(0, 1);
            picture.append(&mut source, 0);
            let mut slice = Bits::default();
            slice.u(2, 5);
            slice.u(0, 1);
            // Two successive +1 forward / -1 backward horizontal deltas,
            // followed by a skipped macroblock and a zero-delta coded one.
            // This catches accidental scaling of the stored predictor itself.
            for index in 0..3 {
                slice.vlc(if index == 2 { "011" } else { "1" });
                slice.vlc(if kind == 2 { "001" } else { "10" });
                slice.vlc(if index == 2 { "1" } else { "010" });
                slice.vlc("1"); // zero vertical delta
                if kind == 3 {
                    slice.vlc(if index == 2 { "1" } else { "011" });
                    slice.vlc("1");
                }
            }
            slice.append(&mut source, 1);
            source.extend_from_slice(&[0, 0, 1, 0xb7]);
            let pictures = parse_elementary_stream(&source).expect("motion picture parses");
            let picture = &pictures[0];
            assert!(picture.is_mpeg1);
            let mut grid = MacroblockGrid::new();
            grid.reset(4);
            decode_slice(
                &mut BitReader::new(&source),
                picture,
                &picture.slices[0],
                4,
                &mut grid,
            )
            .expect("motion slice decodes");
            let forward_scale = if full_forward { 2 } else { 1 };
            let backward_scale = if full_backward { 2 } else { 1 };
            for address in 0..4 {
                let mb = grid.get(address).expect("all four macroblocks present");
                assert_eq!(mb.skipped, address == 2);
                let forward = if kind == 2 && address >= 2 {
                    0
                } else {
                    (address as i32 + 1).min(2) * forward_scale
                };
                let backward = if kind == 3 {
                    -(address as i32 + 1).min(2) * backward_scale
                } else {
                    0
                };
                assert_eq!(
                    &mb.mv[..4],
                    &[forward, 0, backward, 0],
                    "kind={kind}, fullpel=({full_forward},{full_backward}), MB={address}"
                );
            }
        }
    }
}

#[test]
fn mpeg1_pixel_aspect_ratio_reaches_the_timeline() {
    use mpeg2toh264::mpeg2::headers::{
        parse_elementary_stream, picture_sequence_description, stream_sequence_description,
    };
    let mut source = elementary_stream();
    // The fourth sequence-header byte holds aspect_ratio_information and
    // frame_rate_code; MPEG-1 code 3 is a pixel height/width ratio of 0.7031.
    source[7] = (3 << 4) | (source[7] & 15);
    let pictures = parse_elementary_stream(&source).expect("parses");
    let description = picture_sequence_description(&pictures[0]);
    let sar = description.sample_aspect_ratio.expect("non-square pixels");
    assert_eq!((sar.width, sar.height), (10000, 7031));
    assert_eq!(
        stream_sequence_description(&source).expect("description"),
        description
    );
    let timeline = mpeg2_video_timeline(&source, false, &[]).expect("timeline");
    assert_eq!(timeline.sample_aspect_ratio, Some(sar));
}
