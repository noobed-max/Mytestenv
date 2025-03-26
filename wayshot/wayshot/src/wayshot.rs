use std::{
    env,
    io::{self, BufWriter, Cursor, Write},
    process::Command,
};

use clap::Parser;
use eyre::{Result, bail};
use libwayshot::{WayshotConnection, region::LogicalRegion};

mod cli;
mod utils;

use dialoguer::{FuzzySelect, theme::ColorfulTheme};
use utils::EncodingFormat;

use wl_clipboard_rs::copy::{MimeType, Options, Source};

use rustix::runtime::{self, Fork};

fn select_output<T>(outputs: &[T]) -> Option<usize>
where
    T: ToString,
{
    let Ok(selection) = FuzzySelect::with_theme(&ColorfulTheme::default())
        .with_prompt("Choose Screen")
        .default(0)
        .items(outputs)
        .interact()
    else {
        return None;
    };
    Some(selection)
}

fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    tracing_subscriber::fmt()
        .with_max_level(cli.log_level)
        .with_writer(io::stderr)
        .init();

    let input_encoding = cli
        .file
        .as_ref()
        .and_then(|pathbuf| pathbuf.try_into().ok());
    let encoding = cli
        .encoding
        .or(input_encoding)
        .unwrap_or(EncodingFormat::default());

    if let Some(ie) = input_encoding {
        if ie != encoding {
            tracing::warn!(
                "The encoding requested '{encoding}' does not match the output file's encoding '{ie}'. Still using the requested encoding however.",
            );
        }
    }

    let file_name_format = cli
        .file_name_format
        .unwrap_or("wayshot-%Y_%m_%d-%H_%M_%S".to_string());
    let mut stdout_print = false;
    let file = match cli.file {
        Some(pathbuf) => {
            if pathbuf.to_string_lossy() == "-" {
                stdout_print = true;
                None
            } else {
                Some(utils::get_full_file_name(
                    &pathbuf,
                    &file_name_format,
                    encoding,
                ))
            }
        }
        None => {
            if cli.clipboard {
                None
            } else {
                let current_dir = env::current_dir().unwrap_or_default();
                Some(utils::get_full_file_name(
                    &current_dir,
                    &file_name_format,
                    encoding,
                ))
            }
        }
    };

    let wayshot_conn = WayshotConnection::new()?;

    let stdout = io::stdout();
    let mut writer = BufWriter::new(stdout.lock());

    if cli.list_outputs {
        let valid_outputs = wayshot_conn.get_all_outputs();
        for output in valid_outputs {
            writeln!(writer, "{}", output.name)?;
        }

        writer.flush()?;

        return Ok(());
    }

    let image_buffer = wayshot_conn.screenshot_all(cli.cursor)?;

    let mut image_buf: Option<Cursor<Vec<u8>>> = None;
    if let Some(f) = file {
        image_buffer.save(f)?;
    } else if stdout_print {
        let mut buffer = Cursor::new(Vec::new());
        image_buffer.write_to(&mut buffer, encoding.into())?;
        writer.write_all(buffer.get_ref())?;
        image_buf = Some(buffer);
    }

    if cli.clipboard {
        clipboard_daemonize(match image_buf {
            Some(buf) => buf,
            None => {
                let mut buffer = Cursor::new(Vec::new());
                image_buffer.write_to(&mut buffer, encoding.into())?;
                buffer
            }
        })?;
    }

    Ok(())
}

/// Daemonize and copy the given buffer containing the encoded image to the clipboard
fn clipboard_daemonize(buffer: Cursor<Vec<u8>>) -> Result<()> {
    let mut opts = Options::new();
    match unsafe { runtime::kernel_fork() } {
        // Having the image persistently available on the clipboard requires a wayshot process to be alive.
        // Fork the process with a child detached from the main process and have the parent exit
        Ok(Fork::ParentOf(_)) => {
            return Ok(());
        }
        Ok(Fork::Child(_)) => {
            opts.foreground(true); // Offer the image till something else is available on the clipboard
            opts.copy(
                Source::Bytes(buffer.into_inner().into()),
                MimeType::Autodetect,
            )?;
        }
        Err(e) => {
            tracing::warn!(
                "Fork failed with error: {e}, couldn't offer image on the clipboard persistently.
                 Use a clipboard manager to record screenshot."
            );
            opts.copy(
                Source::Bytes(buffer.into_inner().into()),
                MimeType::Autodetect,
            )?;
        }
    }
    Ok(())
}
