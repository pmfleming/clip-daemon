//! Synthetic multi-MIME producer for disposable Wayland acceptance tests only.
use std::io::Read;

use clap::Parser;
use wl_clipboard_rs::copy::{MimeSource, MimeType, Options, Source};

#[derive(Parser)]
struct Args {
    #[arg(long, default_value = "text/plain")]
    mime: String,
    #[arg(long)]
    sensitive: bool,
}

fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let mut bytes = Vec::new();
    std::io::stdin()
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    anyhow::ensure!(
        bytes.len() <= 1024 * 1024,
        "synthetic fixture exceeds 1 MiB"
    );
    let mut sources = vec![MimeSource {
        source: Source::Bytes(bytes.into_boxed_slice()),
        mime_type: MimeType::Specific(args.mime),
    }];
    if args.sensitive {
        sources.push(MimeSource {
            source: Source::Bytes(b"secret".to_vec().into_boxed_slice()),
            mime_type: MimeType::Specific("x-kde-passwordManagerHint".into()),
        });
    }
    let mut options = Options::new();
    options
        .foreground(true)
        .omit_additional_text_mime_types(true);
    options.copy_multi(sources)?;
    Ok(())
}
