use std::{io::{LineWriter, stderr}, time::Duration};

use anyhow::{Result, bail};
use crossterm::{cursor::{RestorePosition, SavePosition}, execute, style::Print, terminal::{disable_raw_mode, enable_raw_mode}};
use scopeguard::defer;
use tokio::{io::{AsyncReadExt, BufReader}, time::{sleep, timeout}};
use tracing::{debug, error, warn};
use yazi_shared::Either;

use crate::{Adapter, Brand, Mux, TMUX, Unknown};

#[derive(Clone, Copy, Debug)]
pub struct Emulator {
	pub kind:      Either<Brand, Unknown>,
	pub light:     bool,
	pub cell_size: Option<(u16, u16)>,
}

impl Default for Emulator {
	fn default() -> Self { Self::unknown() }
}

impl Emulator {
	pub fn detect() -> Result<Self> {
		defer! { disable_raw_mode().ok(); }
		enable_raw_mode()?;

		let resort = Brand::from_env();
		let kgp_seq = if resort.is_none() {
			Mux::csi("\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\")
		} else {
			"".into()
		};

		execute!(
			LineWriter::new(stderr()),
			SavePosition,
			Print(kgp_seq),             // Detect KGP
			Print(Mux::csi("\x1b[>q")), // Request terminal version
			Print("\x1b[16t"),          // Request cell size
			Print("\x1b]11;?\x07"),     // Request background color
			Print(Mux::csi("\x1b[0c")), // Request device attributes
			RestorePosition
		)?;

		let resp = futures::executor::block_on(Self::read_until_da1());
		Mux::tmux_drain()?;

		let kind = if let Some(b) = Brand::from_csi(&resp).or(resort) {
			Either::Left(b)
		} else {
			Either::Right(Unknown {
				kgp:   resp.contains("\x1b_Gi=31;OK"),
				sixel: ["?4;", "?4c", ";4;", ";4c"].iter().any(|s| resp.contains(s)),
			})
		};

		Ok(Self {
			kind,
			light: Self::light_bg(&resp).unwrap_or_default(),
			cell_size: Self::cell_size(&resp),
		})
	}

	pub const fn unknown() -> Self {
		Self { kind: Either::Right(Unknown::default()), light: false, cell_size: None }
	}

	pub fn adapters(self) -> &'static [Adapter] {
		match self.kind {
			Either::Left(brand) => brand.adapters(),
			Either::Right(unknown) => unknown.adapters(),
		}
	}

	pub fn move_lock<F, T>((x, y): (u16, u16), cb: F) -> Result<T>
	where
		F: FnOnce(&mut std::io::BufWriter<std::io::StderrLock>) -> Result<T>,
	{
		use std::{io::Write, thread, time::Duration};

		use crossterm::{cursor::{Hide, MoveTo, RestorePosition, SavePosition, Show}, queue};

		let mut buf = std::io::BufWriter::new(stderr().lock());

		// I really don't want to add this,
		// But tmux and ConPTY sometimes cause the cursor position to get out of sync.
		if TMUX.get() || cfg!(windows) {
			execute!(buf, SavePosition, MoveTo(x, y), Show)?;
			execute!(buf, MoveTo(x, y), Show)?;
			execute!(buf, MoveTo(x, y), Show)?;
			thread::sleep(Duration::from_millis(1));
		} else {
			queue!(buf, SavePosition, MoveTo(x, y))?;
		}

		let result = cb(&mut buf);
		if TMUX.get() || cfg!(windows) {
			queue!(buf, Hide, RestorePosition)?;
		} else {
			queue!(buf, RestorePosition)?;
		}

		buf.flush()?;
		result
	}

	pub async fn read_until_da1() -> String {
		let mut buf: Vec<u8> = Vec::with_capacity(200);
		let read = async {
			let mut stdin = BufReader::new(tokio::io::stdin());
			loop {
				let mut c = [0; 1];
				if stdin.read(&mut c).await? == 0 {
					bail!("unexpected EOF");
				}
				buf.push(c[0]);
				if c[0] != b'c' || !buf.contains(&0x1b) {
					continue;
				}
				if buf.rsplitn(2, |&b| b == 0x1b).next().is_some_and(|s| s.starts_with(b"[?")) {
					break;
				}
			}
			Ok(())
		};

		let h = tokio::spawn(async move {
			sleep(Duration::from_millis(300)).await;
			Self::error_to_user().ok();
		});

		match timeout(Duration::from_secs(2), read).await {
			Ok(Ok(())) => debug!("read_until_da1: {buf:?}"),
			Err(e) => error!("read_until_da1 timed out: {buf:?}, error: {e:?}"),
			Ok(Err(e)) => error!("read_until_da1 failed: {buf:?}, error: {e:?}"),
		}

		h.abort();
		String::from_utf8_lossy(&buf).into_owned()
	}

	pub async fn read_until_dsr() -> String {
		let mut buf: Vec<u8> = Vec::with_capacity(200);
		let read = async {
			let mut stdin = BufReader::new(tokio::io::stdin());
			loop {
				let mut c = [0; 1];
				if stdin.read(&mut c).await? == 0 {
					bail!("unexpected EOF");
				}
				buf.push(c[0]);
				if c[0] == b'n' && (buf.ends_with(b"\x1b[0n") || buf.ends_with(b"\x1b[3n")) {
					break;
				}
			}
			Ok(())
		};

		match timeout(Duration::from_millis(500), read).await {
			Ok(Ok(())) => debug!("read_until_dsr: {buf:?}"),
			Err(e) => error!("read_until_dsr timed out: {buf:?}, error: {e:?}"),
			Ok(Err(e)) => error!("read_until_dsr failed: {buf:?}, error: {e:?}"),
		}
		String::from_utf8_lossy(&buf).into_owned()
	}

	fn error_to_user() -> Result<(), std::io::Error> {
		use crossterm::style::{Attribute, Color, Print, ResetColor, SetAttributes, SetForegroundColor};
		crossterm::execute!(
			std::io::stderr(),
			SetForegroundColor(Color::Red),
			SetAttributes(Attribute::Bold.into()),
			Print("\r\nTerminal response timeout: "),
			ResetColor,
			SetAttributes(Attribute::Reset.into()),
			//
			Print("The request sent by Yazi didn't receive a correct response.\r\n"),
			Print(
				"Please check your terminal environment as per: https://yazi-rs.github.io/docs/faq#trt\r\n"
			),
		)
	}

	fn cell_size(resp: &str) -> Option<(u16, u16)> {
		let b = resp.split_once("\x1b[6;")?.1.as_bytes();

		let h: Vec<_> = b.iter().copied().take_while(|&c| c.is_ascii_digit()).collect();
		b.get(h.len()).filter(|&&c| c == b';')?;

		let w: Vec<_> = b[h.len() + 1..].iter().copied().take_while(|&c| c.is_ascii_digit()).collect();
		b.get(h.len() + 1 + w.len()).filter(|&&c| c == b't')?;

		let (w, h) = unsafe { (String::from_utf8_unchecked(w), String::from_utf8_unchecked(h)) };
		Some((w.parse().ok()?, h.parse().ok()?))
	}

	fn light_bg(resp: &str) -> Result<bool> {
		match resp.split_once("]11;rgb:") {
			Some((_, s)) if s.len() >= 14 => {
				let r = u8::from_str_radix(&s[0..2], 16)? as f32;
				let g = u8::from_str_radix(&s[5..7], 16)? as f32;
				let b = u8::from_str_radix(&s[10..12], 16)? as f32;
				let luma = r * 0.2627 / 256.0 + g * 0.6780 / 256.0 + b * 0.0593 / 256.0;
				debug!("Detected background color: {} (luma = {luma:.2})", &s[..14]);
				Ok(luma > 0.6)
			}
			_ => {
				warn!("Failed to detect background color: {resp:?}");
				Ok(false)
			}
		}
	}
}
