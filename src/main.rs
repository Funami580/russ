#![forbid(unsafe_code)]

use crate::modes::{Mode, Selected};
use anyhow::Result;
use app::App;
use crossterm::event;
use crossterm::event::{Event as CEvent, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use futures_util::StreamExt;
use std::io::stdout;
use std::path::PathBuf;
use std::sync::mpsc;
use std::{thread, time};
use structopt::StructOpt;
use tui::backend::CrosstermBackend;
use tui::Terminal;

mod app;
mod modes;
mod rss;
mod ui;
mod util;

const RUSS_VERSION: &str = env!("RUSS_VERSION");

pub enum Event<I> {
    Input(I),
    Tick,
}

#[derive(Clone, Debug, StructOpt)]
#[structopt(name = "russ", version = crate::RUSS_VERSION)]
pub struct Options {
    /// feed database path
    #[structopt(short, long)]
    database_path: PathBuf,
    /// time in ms between two ticks
    #[structopt(short, long, default_value = "250")]
    tick_rate: u64,
    /// number of seconds to show the flash message before clearing it
    #[structopt(short, long, default_value = "4", parse(try_from_str = parse_seconds))]
    flash_display_duration_seconds: time::Duration,
    /// RSS/Atom network request timeout in seconds
    #[structopt(short, long, default_value = "5", parse(try_from_str = parse_seconds))]
    network_timeout: time::Duration,
}

fn parse_seconds(s: &str) -> Result<time::Duration, std::num::ParseIntError> {
    let as_u64 = s.parse::<u64>()?;
    Ok(time::Duration::from_secs(as_u64))
}

enum IoCommand {
    Break,
    RefreshFeed(crate::rss::FeedId),
    RefreshFeeds(Vec<crate::rss::FeedId>),
    SubscribeToFeed(String),
    ClearFlash,
}

async fn async_io_loop(
    app: App,
    sx: &mpsc::Sender<IoCommand>,
    rx: mpsc::Receiver<IoCommand>,
    options: &Options,
) -> Result<()> {
    use IoCommand::*;

    let manager = r2d2_sqlite::SqliteConnectionManager::file(&options.database_path);
    let connection_pool = r2d2::Pool::new(manager)?;

    while let Ok(event) = rx.recv() {
        match event {
            Break => break,
            RefreshFeed(feed_id) => {
                let now = std::time::Instant::now();

                app.set_flash("Refreshing feed...".to_string());
                app.force_redraw()?;

                refresh_feeds(&app, &connection_pool, &[feed_id], |_app, fetch_result| {
                    if let Err(e) = fetch_result {
                        app.push_error_flash(e)
                    }
                })
                .await?;

                app.update_current_feed_and_entries()?;
                let elapsed = now.elapsed();
                app.set_flash(format!("Refreshed feed in {:?}", elapsed));
                app.force_redraw()?;
                clear_flash_after(sx, &options.flash_display_duration_seconds).await;
            }
            RefreshFeeds(feed_ids) => {
                let now = std::time::Instant::now();

                app.set_flash("Refreshing all feeds...".to_string());
                app.force_redraw()?;

                let all_feeds_len = feed_ids.len();
                let mut successfully_refreshed_len = 0usize;

                refresh_feeds(&app, &connection_pool, &feed_ids, |app, fetch_result| {
                    match fetch_result {
                        Ok(_) => successfully_refreshed_len += 1,
                        Err(e) => app.push_error_flash(e),
                    }
                })
                .await?;

                {
                    app.update_current_feed_and_entries()?;

                    let elapsed = now.elapsed();
                    app.set_flash(format!(
                        "Refreshed {}/{} feeds in {:?}",
                        successfully_refreshed_len, all_feeds_len, elapsed
                    ));
                    app.force_redraw()?;
                }

                clear_flash_after(sx, &options.flash_display_duration_seconds).await;
            }
            SubscribeToFeed(feed_subscription_input) => {
                let now = std::time::Instant::now();

                app.set_flash("Subscribing to feed...".to_string());
                app.force_redraw()?;

                let conn = connection_pool.get()?;
                let r = crate::rss::subscribe_to_feed(
                    &app.http_client(),
                    &conn,
                    &feed_subscription_input,
                );

                if let Err(e) = r {
                    app.push_error_flash(e);
                    continue;
                }

                match crate::rss::get_feeds(&conn) {
                    Ok(feeds) => {
                        {
                            app.reset_feed_subscription_input();
                            app.set_feeds(feeds);
                            app.select_feeds();
                            app.update_current_feed_and_entries()?;

                            let elapsed = now.elapsed();
                            app.set_flash(format!("Subscribed in {:?}", elapsed));
                            app.force_redraw()?;
                        }

                        clear_flash_after(sx, &options.flash_display_duration_seconds).await;
                    }
                    Err(e) => {
                        app.push_error_flash(e);
                    }
                }
            }
            ClearFlash => {
                app.clear_flash();
            }
        }
    }

    Ok(())
}

async fn refresh_feeds<'a, F>(
    app: &App,
    connection_pool: &r2d2::Pool<r2d2_sqlite::SqliteConnectionManager>,
    feed_ids: &[crate::rss::FeedId],
    mut f: F,
) -> Result<()>
where
    F: FnMut(&App, anyhow::Result<()>),
{
    let feed_ids = feed_ids.to_owned();
    let requests_stream = futures_util::stream::iter(feed_ids).map(|feed_id| {
        let pool_get_result = connection_pool.get();
        let http = app.http_client();
        // `tokio::task::spawn_blocking` here because the http client `ureq` is blocking,
        // and using `tokio::task::spawn` with a blocking call has the potential to block
        // the scheduler
        tokio::task::spawn_blocking(move || {
            let conn = pool_get_result?;
            crate::rss::refresh_feed(&http, &conn, feed_id)?;
            Ok(())
        })
    });

    let mut buffered_requests = requests_stream.buffer_unordered(num_cpus::get() * 2);

    while let Some(task_join_result) = buffered_requests.next().await {
        let fetch_result = task_join_result?;
        f(app, fetch_result)
    }

    Ok(())
}

async fn clear_flash_after(sx: &mpsc::Sender<IoCommand>, duration: &time::Duration) {
    tokio::time::sleep(*duration).await;
    sx.send(IoCommand::ClearFlash)
        .expect("Unable to send IOCommand::ClearFlash");
}

fn main() -> Result<()> {
    let options: Options = Options::from_args();

    enable_raw_mode()?;

    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;

    let backend = CrosstermBackend::new(stdout);

    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    // Setup input handling
    let (tx, rx) = mpsc::channel();
    let tx_clone = tx.clone();

    let tick_rate = time::Duration::from_millis(options.tick_rate);
    thread::spawn(move || {
        let mut last_tick = time::Instant::now();
        loop {
            // poll for tick rate duration, if no events, sent tick event.
            if event::poll(tick_rate - last_tick.elapsed())
                .expect("Unable to poll for Crossterm event")
            {
                if let CEvent::Key(key) = event::read().expect("Unable to read Crossterm event") {
                    tx.send(Event::Input(key))
                        .expect("Unable to send Crossterm Key input event");
                }
            }
            if last_tick.elapsed() >= tick_rate {
                tx.send(Event::Tick).expect("Unable to send tick");
                last_tick = time::Instant::now();
            }
        }
    });

    let options_clone = options.clone();

    let app = App::new(options, tx_clone)?;

    let cloned_app = app.clone();

    terminal.clear()?;

    let (io_s, io_r) = mpsc::channel();

    let io_s_clone = io_s.clone();

    // we run tokio in this thread to manage the blocking http calls used to fetch feeds
    let io_thread = thread::spawn(move || -> Result<()> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;

        rt.block_on(async move {
            async_io_loop(cloned_app, &io_s_clone, io_r, &options_clone).await?;
            Ok(())
        })
    });

    // MAIN THREAD IS DRAW THREAD
    loop {
        let mode = {
            app.draw(&mut terminal)?;
            app.mode()
        };

        match mode {
            Mode::Normal => match rx.recv()? {
                Event::Input(event) => match (event.code, event.modifiers) {
                    // These first few keycodes are handled inline
                    // because they talk to either the IO thread or the terminal.
                    // All other keycodes are handled in the final `on_key`
                    // wildcard pattern, as they do neither.
                    (KeyCode::Char('q'), _)
                    | (KeyCode::Char('c'), KeyModifiers::CONTROL)
                    | (KeyCode::Esc, _) => {
                        if !app.error_flash_is_empty() {
                            app.clear_error_flash();
                        } else {
                            disable_raw_mode()?;
                            execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                            terminal.show_cursor()?;
                            io_s.send(IoCommand::Break)?;
                            break;
                        }
                    }
                    (KeyCode::Char('r'), KeyModifiers::NONE) => match &app.selected() {
                        Selected::Feeds => {
                            let feed_id = app.selected_feed_id();
                            io_s.send(IoCommand::RefreshFeed(feed_id))?;
                        }
                        _ => app.toggle_read()?,
                    },
                    (KeyCode::Char('x'), KeyModifiers::NONE) => {
                        let feed_ids = app.feed_ids()?;
                        io_s.send(IoCommand::RefreshFeeds(feed_ids))?;
                    }
                    // handle all other normal-mode keycodes here
                    (keycode, modifiers) => {
                        // Manually match out the on_key result here
                        // and show errors in the error flash,
                        // because these on_key actions can fail
                        // in such a way that the app can continue.
                        if let Err(e) = app.on_key(keycode, modifiers) {
                            app.push_error_flash(e);
                        }
                    }
                },
                Event::Tick => (),
            },
            Mode::Editing => match rx.recv()? {
                Event::Input(event) => match event.code {
                    KeyCode::Enter => {
                        let feed_subscription_input = { app.feed_subscription_input() };
                        io_s.send(IoCommand::SubscribeToFeed(feed_subscription_input))?;
                    }
                    KeyCode::Char(c) => {
                        app.push_feed_subscription_input(c);
                    }
                    KeyCode::Backspace => app.pop_feed_subscription_input(),
                    KeyCode::Delete => {
                        app.delete_feed()?;
                    },
                    KeyCode::Esc => {
                        app.set_mode(Mode::Normal);
                    }
                    _ => {}
                },
                Event::Tick => (),
            },
        }
    }

    io_thread
        .join()
        .expect("Unable to join IO thread to main thread")?;

    Ok(())
}
