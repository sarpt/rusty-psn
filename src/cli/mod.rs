use std::io::Write;
use std::path::PathBuf;

use bytesize::ByteSize;
use crossterm::{cursor, terminal};
use poll_promise::Promise;
use tokio::runtime::Runtime;

use crate::psn::*;
use crate::Args;

pub fn start_app(args: Args) {
    let runtime = Runtime::new().unwrap();

    let _guard = runtime.enter();

    let titles = args.titles[0].split(' ');
    let silent_mode = args.silent;
    let destination_path = args.destination_path.unwrap_or_else(|| PathBuf::from("pkgs/"));

    if silent_mode {
        info!("App started in silent mode!");
    }

    let update_info = {
        let mut info = Vec::new();

        let promises = titles
            .into_iter()
            .map(|t| (t.to_string(), Promise::spawn_async(UpdateInfo::get_info(t.to_string()))))
            .collect::<Vec<(String, Promise<Result<UpdateInfo, UpdateError>>)>>();

        if !silent_mode {
            println!("Searching for updates...\n");
        }

        for (id, promise) in promises {
            info!("Checking in on search promises");

            match promise.block_and_take() {
                Ok(i) => {
                    info!("Successfully search for updates for {id}");
                    info.push(i);
                }
                Err(e) => match e {
                    UpdateError::UnhandledErrorResponse(e) => {
                        error!("Unexpected error received in response from PSN: {e}");
                        println!("{id}: PSN returned an unexpected error: {e}.");
                    }
                    UpdateError::InvalidSerial => {
                        error!("Invalid serial for updates query {id}");
                        println!("{id}: The provided serial didn't give any results, double-check your input.");
                    }
                    UpdateError::NoUpdatesAvailable => {
                        warn!("No updates available for serial {id}");
                        println!("{id}: The provided serial doesn't have any available updates.");
                    }
                    UpdateError::Reqwest(e) => {
                        error!("reqwest error on updates query: {e}");
                        println!("{id}: There was an error on the request: {e}.");
                    }
                    UpdateError::XmlParsing(e) => {
                        error!("Failed to deserialize response for {id}: {e}");
                        println!("{id}: Error parsing response from PSN, try again later ({e}).");
                    }
                    UpdateError::ManifestParsing(e) => {
                        error!("Failed to deserialize manifest response for {id}: {e}");
                        println!("{id}: Error parsing manifest response from PSN, try again later ({e}).");
                    }
                },
            }
        }

        info
    };

    for update in update_info {
        let title = {
            if let Some(title) = update.titles.first() {
                title.clone()
            } else {
                warn!("Failed to get update's title: Last pkg's info didn't contain a title");
                String::from("Untitled")
            }
        };

        if !silent_mode {
            crossterm::execute!(
                std::io::stdout(),
                terminal::Clear(terminal::ClearType::All),
                cursor::MoveTo(0, 0)
            )
            .unwrap();

            let total_size = {
                let mut total = 0;

                for pkg in update.packages.iter() {
                    total += pkg.size;
                }

                ByteSize::b(total)
            };

            println!(
                "[{}] {} - {} - {} update(s) ({})",
                update.platform_variant,
                update.title_id,
                &title,
                update.packages.len(),
                total_size
            );

            for (i, pkg) in update.packages.iter().enumerate() {
                println!("  {i}. {} ({})", pkg.id(), ByteSize::b(pkg.size));
            }
        }

        let mut response = String::new();
        let mut updates_to_fetch = Vec::new();

        if !silent_mode {
            info!("Querying user for wanted updates for {}", update.title_id);
            println!("\nEnter the updates you want to download, separated by a space (ie: 1 3 4 5). An empty input will download all updates.");

            std::io::stdin().read_line(&mut response).unwrap();
            response = response.trim().to_string();

            info!("User input was '{}'", response);

            if !response.is_empty() {
                updates_to_fetch = response
                    .split(' ')
                    .filter_map(|s| s.parse::<usize>().ok())
                    .filter(|idx| *idx < update.packages.len())
                    .collect();

                updates_to_fetch.sort_unstable();
                updates_to_fetch.dedup();
            }

            let updates = {
                let mut updates = String::new();

                if updates_to_fetch.is_empty() {
                    for (i, pkg) in update.packages.iter().enumerate() {
                        updates.push_str(&pkg.id());

                        if i < update.packages.len() - 1 {
                            updates.push_str(", ");
                        }
                    }
                } else {
                    for (i, update_idx) in updates_to_fetch.iter().enumerate() {
                        updates.push_str(&update.packages[*update_idx].id().to_string());

                        if i < updates_to_fetch.len() - 1 {
                            updates.push_str(", ");
                        }
                    }
                }

                updates
            };

            info!("Downloading updates {updates}");

            crossterm::execute!(
                std::io::stdout(),
                terminal::Clear(terminal::ClearType::All),
                cursor::MoveTo(0, 0)
            )
            .unwrap();
            println!("{} {} - Downloading update(s): {}", update.title_id, title, updates);
        }

        for (idx, pkg) in update.packages.iter().enumerate() {
            if !updates_to_fetch.is_empty() && !updates_to_fetch.contains(&idx) {
                continue;
            }

            let (tx, mut rx) = tokio::sync::mpsc::channel(10);
            let serial = update.title_id.clone();
            let download_path = destination_path.clone();

            let dpkg = pkg.clone();
            let dtitle = title.clone();

            let promise = Promise::spawn_async(async move { dpkg.start_download(tx, download_path, serial, dtitle).await });

            let mut stdout = std::io::stdout();
            let mut downloaded = 0;

            crossterm::execute!(stdout, cursor::SavePosition).unwrap();

            loop {
                match promise.ready() {
                    Some(result) => {
                        if let Err(e) = result {
                            match e {
                                DownloadError::HashMismatch(short_on_data) => {
                                    error!(
                                        "Download of {} {} failed: hash mismatch. (short on data: {})",
                                        update.title_id,
                                        pkg.id(),
                                        short_on_data
                                    );
                                    println!("Error downloading update: hash mismatch on downloaded file.");

                                    if *short_on_data {
                                        println!("The downloaded file is smaller than expected. Please try again later, as Sony's servers can sometimes be unreliable");
                                    }
                                }
                                DownloadError::Tokio(e) => {
                                    error!("Download of {} {} failed: {e}", update.title_id, pkg.id());
                                    println!("Error downloading update: {e}.")
                                }
                                DownloadError::Reqwest(e) => {
                                    error!("Download of {} {} failed: {e}", update.title_id, pkg.id());
                                    println!("Error downloading update: {e}.")
                                }
                            }
                        }

                        break;
                    }
                    None => {
                        if let Ok(status) = rx.try_recv() {
                            match status {
                                DownloadStatus::Progress(bytes) => {
                                    downloaded += bytes;

                                    if !silent_mode {
                                        crossterm::execute!(
                                            stdout,
                                            cursor::RestorePosition,
                                            terminal::Clear(terminal::ClearType::CurrentLine),
                                            cursor::SavePosition
                                        )
                                        .unwrap();
                                        print!(
                                            "        {} - {title} | {} / {}",
                                            pkg.id(),
                                            ByteSize::b(downloaded),
                                            ByteSize::b(pkg.size)
                                        );
                                        stdout.flush().unwrap();
                                    }
                                }
                                DownloadStatus::Verifying => {
                                    if !silent_mode {
                                        crossterm::execute!(
                                            stdout,
                                            cursor::RestorePosition,
                                            terminal::Clear(terminal::ClearType::CurrentLine),
                                            cursor::SavePosition
                                        )
                                        .unwrap();
                                        print!("        {} - {title} | Verifying checksum... ", pkg.id());
                                        stdout.flush().unwrap();
                                    }
                                }
                                DownloadStatus::DownloadSuccess => {
                                    if !silent_mode {
                                        crossterm::execute!(
                                            stdout,
                                            cursor::RestorePosition,
                                            terminal::Clear(terminal::ClearType::CurrentLine),
                                            cursor::SavePosition
                                        )
                                        .unwrap();
                                        println!("        {} - {title} | Download completed successfully. ", pkg.id());
                                        stdout.flush().unwrap();
                                    }
                                }
                                DownloadStatus::DownloadFailure => {
                                    if !silent_mode {
                                        crossterm::execute!(
                                            stdout,
                                            cursor::RestorePosition,
                                            terminal::Clear(terminal::ClearType::CurrentLine),
                                            cursor::SavePosition
                                        )
                                        .unwrap();
                                        println!("        {} - {title} | Download failed. ", pkg.id());
                                        stdout.flush().unwrap();
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_secs(3));

        if !silent_mode {
            crossterm::execute!(
                std::io::stdout(),
                terminal::Clear(terminal::ClearType::All),
                cursor::MoveTo(0, 0)
            )
            .unwrap();
        }
    }
}
