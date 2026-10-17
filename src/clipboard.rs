//! Puts text on the clipboard of Windows.

use std::mem;

use anyhow::{Result, bail};
use windows::Win32::{
    Foundation::{GlobalFree, HANDLE, HGLOBAL},
    System::{
        DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
        Ole::CF_UNICODETEXT,
    },
};

pub fn copy(text: &str) -> Result<()> {
    let units: Vec<u16> = text.encode_utf16().chain(Some(0)).collect();
    let bytes = units.len() * mem::size_of::<u16>();
    // SAFETY: the memory block is written within its size before the clipboard takes it over. If
    // the clipboard does not take it, it is freed here.
    unsafe {
        OpenClipboard(None)?;
        let result = (|| -> Result<()> {
            EmptyClipboard()?;
            let block: HGLOBAL = GlobalAlloc(GMEM_MOVEABLE, bytes)?;
            let target = GlobalLock(block).cast::<u16>();
            if target.is_null() {
                let _ = GlobalFree(Some(block));
                bail!("cannot lock the memory for the clipboard");
            }
            std::ptr::copy_nonoverlapping(units.as_ptr(), target, units.len());
            let _ = GlobalUnlock(block);
            if let Err(e) = SetClipboardData(u32::from(CF_UNICODETEXT.0), Some(HANDLE(block.0))) {
                let _ = GlobalFree(Some(block));
                return Err(e.into());
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replaces the clipboard, so it is not run by default. Run it with
    /// `cargo test --release copies_text_with_any_character -- --ignored`.
    #[test]
    #[ignore]
    fn copies_text_with_any_character() {
        let text = "problem: /data/视频 — ñ 😀";
        copy(text).unwrap();
        let output = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "[Console]::OutputEncoding = [Text.Encoding]::UTF8; Get-Clipboard -Raw",
            ])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), text);
    }
}
