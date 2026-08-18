//! Packing a sequence of rendered frames into one animated file.
//!
//! APNG is used rather than a video container because it needs no encoder
//! beyond the PNG writer already in the project, keeps full colour with no
//! block artefacts, and plays in any browser and in most image viewers. GIF is
//! the default because it also plays outside one.
//!
//! Both encoders are fed in batches rather than handed the whole window at
//! once. A frame is four bytes a pixel however well it compresses, so a long
//! export at full size is gigabytes of RGBA; holding a batch at a time keeps
//! the peak flat no matter how many frames are asked for.

/// Frames must all share these dimensions.
pub struct Animation {
    pub width: usize,
    pub height: usize,
    /// Milliseconds each frame is held.
    pub delay_ms: u16,
}

/// How many bytes one frame occupies as RGBA.
pub fn frame_bytes(width: usize, height: usize) -> usize {
    width.saturating_mul(height).saturating_mul(4)
}

impl Animation {
    /// Encode RGBA frames, oldest first.
    pub fn encode(&self, frames: &[Vec<u8>]) -> Result<Vec<u8>, String> {
        if frames.is_empty() {
            return Err("no frames to encode".into());
        }
        let mut w = self.writer(Format::Apng, frames.len())?;
        w.push(&mut frames.to_vec())?;
        w.finish()
    }

    /// Encode the same frames as a GIF.
    ///
    /// APNG keeps full colour but only animates inside a browser - Windows
    /// Photos, Explorer previews and most desktop viewers show a still frame -
    /// so GIF is the default despite being limited to 256 colours per frame.
    pub fn encode_gif(&self, frames: &[Vec<u8>]) -> Result<Vec<u8>, String> {
        if frames.is_empty() {
            return Err("no frames to encode".into());
        }
        let mut w = self.writer(Format::Gif, frames.len())?;
        w.push(&mut frames.to_vec())?;
        w.finish()
    }

    /// Start a streaming encoder. `total` must be the exact number of frames
    /// that will be pushed: APNG records the count in its header.
    pub fn writer(&self, format: Format, total: usize) -> Result<Writer, String> {
        if total == 0 {
            return Err("no frames to encode".into());
        }
        if format == Format::Gif
            && (self.width > u16::MAX as usize || self.height > u16::MAX as usize)
        {
            return Err("frame is too large for GIF".into());
        }
        Ok(Writer {
            width: self.width,
            height: self.height,
            delay_ms: self.delay_ms,
            format,
            total,
            written: 0,
            sink: Sink::new(),
            png: None,
            gif: None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Gif,
    Apng,
}

/// A byte sink the encoder can own while the caller keeps hold of the buffer.
///
/// `png::Writer::finish` consumes the writer and returns nothing, so there is
/// no way to get a borrowed `Vec` back out of it; sharing the buffer instead
/// keeps the encoder alive across batches without making `Writer`
/// self-referential.
#[derive(Clone)]
struct Sink(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl Sink {
    fn new() -> Sink {
        Sink(std::sync::Arc::new(std::sync::Mutex::new(Vec::new())))
    }

    fn take(&self) -> Vec<u8> {
        std::mem::take(&mut self.0.lock().unwrap())
    }
}

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// An encoder being fed a batch at a time.
pub struct Writer {
    width: usize,
    height: usize,
    delay_ms: u16,
    format: Format,
    total: usize,
    written: usize,
    sink: Sink,
    png: Option<png::Writer<Sink>>,
    gif: Option<gif::Encoder<Sink>>,
}

impl Writer {
    /// Append a batch of RGBA frames, oldest first.
    ///
    /// The buffers are taken mutably because GIF quantisation works in place;
    /// they hold no useful content afterwards. Their contents are consumed, so
    /// the caller can drop them straight away and keep the peak down.
    pub fn push(&mut self, frames: &mut [Vec<u8>]) -> Result<(), String> {
        let expected = frame_bytes(self.width, self.height);
        if let Some(bad) = frames.iter().position(|f| f.len() != expected) {
            return Err(format!(
                "frame {bad} is {} bytes, expected {expected}",
                frames[bad].len()
            ));
        }
        if self.written + frames.len() > self.total {
            return Err("more frames than the encoder was told to expect".into());
        }
        match self.format {
            Format::Apng => self.push_apng(frames),
            Format::Gif => self.push_gif(frames),
        }?;
        self.written += frames.len();
        Ok(())
    }

    fn push_apng(&mut self, frames: &[Vec<u8>]) -> Result<(), String> {
        if self.png.is_none() {
            let mut enc =
                png::Encoder::new(self.sink.clone(), self.width as u32, self.height as u32);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.set_compression(png::Compression::Fast);
            enc.set_animated(self.total as u32, 0)
                .map_err(|e| e.to_string())?;
            enc.set_frame_delay(self.delay_ms, 1000)
                .map_err(|e| e.to_string())?;
            self.png = Some(enc.write_header().map_err(|e| e.to_string())?);
        }
        let w = self.png.as_mut().unwrap();
        for f in frames {
            w.write_image_data(f).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn push_gif(&mut self, frames: &mut [Vec<u8>]) -> Result<(), String> {
        if self.gif.is_none() {
            let mut enc = gif::Encoder::new(
                self.sink.clone(),
                self.width as u16,
                self.height as u16,
                &[],
            )
            .map_err(|e| e.to_string())?;
            enc.set_repeat(gif::Repeat::Infinite)
                .map_err(|e| e.to_string())?;
            self.gif = Some(enc);
        }

        /* Quantising to 256 colours is the whole cost of a GIF export - far
        more than rendering the frames when they are already cached - and
        each frame is quantised independently. Doing them across the cores
        and only the writing in order turns the export from serial into
        roughly single-frame time per core. */
        let (w, h, delay) = (
            self.width as u16,
            self.height as u16,
            // GIF delays are in hundredths of a second, and most viewers treat
            // anything under 2 as "as fast as possible".
            (self.delay_ms / 10).max(2),
        );
        let mut built: Vec<Option<gif::Frame<'static>>> = (0..frames.len()).map(|_| None).collect();
        std::thread::scope(|scope| {
            for (slot, pixels) in built.iter_mut().zip(frames.iter_mut()) {
                scope.spawn(move || {
                    let mut f = gif::Frame::from_rgba_speed(w, h, pixels, 10);
                    f.delay = delay;
                    *slot = Some(f);
                });
            }
        });

        let enc = self.gif.as_mut().unwrap();
        for f in built.into_iter().flatten() {
            enc.write_frame(&f).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    pub fn finish(mut self) -> Result<Vec<u8>, String> {
        if self.written != self.total {
            return Err(format!(
                "encoder expected {} frames but was given {}",
                self.total, self.written
            ));
        }
        if let Some(w) = self.png.take() {
            w.finish().map_err(|e| e.to_string())?;
        }
        // The GIF encoder writes its trailer on drop, so the buffer is only
        // complete once it has gone.
        drop(self.gif.take());
        Ok(self.sink.take())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(w: usize, h: usize, v: u8) -> Vec<u8> {
        vec![v; w * h * 4]
    }

    #[test]
    fn encodes_a_gif() {
        let a = Animation {
            width: 8,
            height: 6,
            delay_ms: 125,
        };
        let g = a.encode_gif(&[frame(8, 6, 10), frame(8, 6, 200)]).unwrap();
        assert_eq!(&g[0..3], b"GIF");
        // The application extension block is what carries the loop count.
        assert!(g.windows(11).any(|w| w == b"NETSCAPE2.0"));
    }

    #[test]
    fn encodes_an_animation() {
        let a = Animation {
            width: 8,
            height: 6,
            delay_ms: 100,
        };
        let png = a.encode(&[frame(8, 6, 10), frame(8, 6, 200)]).unwrap();
        assert_eq!(&png[1..4], b"PNG");
        // acTL marks the file as animated.
        assert!(
            png.windows(4).any(|w| w == b"acTL"),
            "expected an APNG control chunk"
        );
    }

    #[test]
    fn rejects_mismatched_frames() {
        let a = Animation {
            width: 8,
            height: 6,
            delay_ms: 100,
        };
        assert!(a.encode(&[frame(8, 6, 1), frame(4, 4, 1)]).is_err());
        assert!(a.encode(&[]).is_err());
    }

    /// Pushing in batches must produce the same file as pushing all at once:
    /// that is what lets the export bound its memory.
    #[test]
    fn batched_and_whole_encodes_agree() {
        for format in [Format::Gif, Format::Apng] {
            let a = Animation {
                width: 8,
                height: 6,
                delay_ms: 100,
            };
            let frames = || vec![frame(8, 6, 10), frame(8, 6, 90), frame(8, 6, 200)];

            let mut whole = a.writer(format, 3).unwrap();
            whole.push(&mut frames()).unwrap();
            let one_go = whole.finish().unwrap();

            let mut batched = a.writer(format, 3).unwrap();
            let f = frames();
            batched.push(&mut f[0..2].to_vec()).unwrap();
            batched.push(&mut f[2..3].to_vec()).unwrap();
            let in_parts = batched.finish().unwrap();

            assert_eq!(one_go, in_parts);
            assert!(!one_go.is_empty());
        }
    }

    #[test]
    fn finish_rejects_an_incomplete_run() {
        let a = Animation {
            width: 4,
            height: 4,
            delay_ms: 100,
        };
        let mut w = a.writer(Format::Gif, 3).unwrap();
        w.push(&mut [frame(4, 4, 1)]).unwrap();
        assert!(w.finish().is_err());
    }
}
