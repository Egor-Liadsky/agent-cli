//! Копирование текста в системный буфер обмена.
//!
//! Основной путь — нативный буфер (`arboard`). Если его нет (например, TUI
//! запущен по ssh без доступа к дисплею), пробуем OSC 52: терминал сам кладёт
//! текст в буфер обмена локальной машины.

use std::io::Write;

pub fn copy(text: &str) -> anyhow::Result<()> {
    match arboard::Clipboard::new().and_then(|mut cb| cb.set_text(text.to_string())) {
        Ok(()) => Ok(()),
        Err(err) => copy_osc52(text).map_err(|_| anyhow::anyhow!(err)),
    }
}

fn copy_osc52(text: &str) -> anyhow::Result<()> {
    let encoded = base64(text.as_bytes());
    let mut out = std::io::stdout();
    write!(out, "\x1b]52;c;{encoded}\x07")?;
    out.flush()?;
    Ok(())
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        let idx = [(n >> 18) & 63, (n >> 12) & 63, (n >> 6) & 63, n & 63];
        out.push(ALPHABET[idx[0] as usize] as char);
        out.push(ALPHABET[idx[1] as usize] as char);
        out.push(if chunk.len() > 1 { ALPHABET[idx[2] as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[idx[3] as usize] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::base64;

    #[test]
    fn base64_pads_partial_chunks() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64("привет".as_bytes()), "0L/RgNC40LLQtdGC");
    }
}
