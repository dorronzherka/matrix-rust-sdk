use std::{
    collections::HashMap,
    env,
    io::{self, stdout, Write},
    path::PathBuf,
    process::exit,
    sync::{Arc, Mutex},
    time::Duration,
};

use color_eyre::config::HookBuilder;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
    ExecutableCommand,
};
use futures_util::{pin_mut, StreamExt as _};
use imbl::Vector;
use matrix_sdk::{
    config::StoreConfig,
    encryption::{BackupDownloadStrategy, EncryptionSettings},
    matrix_auth::MatrixSession,
    ruma::{
        api::client::receipt::create_receipt::v3::ReceiptType, events::room::message::MessageType,
        OwnedRoomId, RoomId,
    },
    AuthSession, Client, RoomListEntry, ServerName, SqliteCryptoStore, SqliteStateStore,
};
use matrix_sdk_ui::{
    room_list_service,
    sync_service::{self, SyncService},
    timeline::{TimelineItem, TimelineItemContent, TimelineItemKind, VirtualTimelineItem},
};
use ratatui::{prelude::*, style::palette::tailwind, widgets::*};
use tokio::{spawn, task::JoinHandle};
use tracing::error;
use tracing_subscriber::{layer::SubscriberExt as _, util::SubscriberInitExt as _, EnvFilter};

const HEADER_BG: Color = tailwind::BLUE.c950;
const NORMAL_ROW_COLOR: Color = tailwind::SLATE.c950;
const ALT_ROW_COLOR: Color = tailwind::SLATE.c900;
const SELECTED_STYLE_FG: Color = tailwind::BLUE.c300;
const TEXT_COLOR: Color = tailwind::SLATE.c200;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .with_writer(tracing_appender::rolling::hourly("/tmp/", "logs-"));

    tracing_subscriber::registry()
        .with(EnvFilter::new(std::env::var("RUST_LOG").unwrap_or("".into())))
        .with(file_layer)
        .init();

    // Read the server name from the command line.
    let Some(server_name) = env::args().nth(1) else {
        eprintln!("Usage: {} <server_name> <session_path?>", env::args().next().unwrap());
        exit(1)
    };

    let config_path = env::args().nth(2).unwrap_or("/tmp/".to_owned());
    let client = configure_client(server_name, config_path).await?;

    init_error_hooks()?;
    let terminal = init_terminal()?;

    let mut app = App::new(client).await?;

    app.run(terminal).await
}

fn init_error_hooks() -> anyhow::Result<()> {
    let (panic, error) = HookBuilder::default().into_hooks();
    let panic = panic.into_panic_hook();
    let error = error.into_eyre_hook();
    color_eyre::eyre::set_hook(Box::new(move |e| {
        let _ = restore_terminal();
        error(e)
    }))?;
    std::panic::set_hook(Box::new(move |info| {
        let _ = restore_terminal();
        panic(info)
    }));
    Ok(())
}

fn init_terminal() -> anyhow::Result<Terminal<impl Backend>> {
    enable_raw_mode()?;
    stdout().execute(EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout());
    let terminal = Terminal::new(backend)?;
    Ok(terminal)
}

fn restore_terminal() -> anyhow::Result<()> {
    disable_raw_mode()?;
    stdout().execute(LeaveAlternateScreen)?;
    Ok(())
}

#[derive(Default)]
struct StatefulList<T> {
    state: ListState,
    items: Arc<Mutex<Vector<T>>>,
}

#[derive(Default, PartialEq)]
enum DetailsMode {
    #[default]
    ReadReceipts,
    TimelineItems,
    // Events // TODO: Soon™
}

struct Timeline {
    items: Arc<Mutex<Vector<Arc<TimelineItem>>>>,
    task: JoinHandle<()>,
}

struct App {
    /// Reference to the main SDK client.
    client: Client,

    /// The sync service used for synchronizing events.
    sync_service: Arc<SyncService>,

    /// Room list service rooms known to the app.
    ui_rooms: Arc<Mutex<HashMap<OwnedRoomId, room_list_service::Room>>>,

    /// Timelines data structures for each room.
    timelines: Arc<Mutex<HashMap<OwnedRoomId, Timeline>>>,

    /// Ratatui's list of room list entries.
    room_list_entries: StatefulList<RoomListEntry>,

    /// Task listening to room list service changes, and spawning timelines.
    listen_task: JoinHandle<()>,

    /// Content of the latest status message, if set.
    last_status_message: Arc<Mutex<Option<String>>>,

    /// A task to automatically clear the status message in N seconds, if set.
    clear_status_message: Option<JoinHandle<()>>,

    /// What's shown in the details view, aka the right panel.
    details_mode: DetailsMode,

    /// The current room that's subscribed to in the room list's sliding sync.
    current_room_subscription: Option<room_list_service::Room>,
}

impl App {
    async fn new(client: Client) -> anyhow::Result<Self> {
        let sync_service = Arc::new(SyncService::builder(client.clone()).build().await?);

        let room_list_service = sync_service.room_list_service();

        let all_rooms = room_list_service.all_rooms().await?;
        let (rooms, stream) = all_rooms.entries();

        let rooms = Arc::new(Mutex::new(rooms));
        let ui_rooms: Arc<Mutex<HashMap<OwnedRoomId, room_list_service::Room>>> =
            Default::default();
        let timelines = Arc::new(Mutex::new(HashMap::new()));

        let r = rooms.clone();
        let ur = ui_rooms.clone();
        let s = sync_service.clone();
        let t = timelines.clone();

        let listen_task = spawn(async move {
            pin_mut!(stream);
            let rooms = r;
            let ui_rooms = ur;
            let sync_service = s;
            let timelines = t;

            while let Some(diffs) = stream.next().await {
                let all_rooms = {
                    // Apply the diffs to the list of room entries.
                    let mut rooms = rooms.lock().unwrap();
                    for diff in diffs {
                        diff.apply(&mut rooms);
                    }

                    // Collect rooms early to release the room entries list lock.
                    rooms
                        .iter()
                        .filter_map(|entry| entry.as_room_id().map(ToOwned::to_owned))
                        .collect::<Vec<_>>()
                };

                // Clone the previous set of ui rooms to avoid keeping the ui_rooms lock (which
                // we couldn't do below, because it's a sync lock, and has to be
                // sync b/o rendering; and we'd have to cross await points
                // below).
                let previous_ui_rooms = ui_rooms.lock().unwrap().clone();

                let mut new_ui_rooms = HashMap::new();
                let mut new_timelines = Vec::new();

                // Initialize all the new rooms.
                for room_id in
                    all_rooms.into_iter().filter(|room_id| !previous_ui_rooms.contains_key(room_id))
                {
                    // Retrieve the room list service's Room.
                    let Ok(ui_room) = sync_service.room_list_service().room(&room_id).await else {
                        error!("error when retrieving room after an update");
                        continue;
                    };

                    // Initialize the timeline.
                    let builder = match ui_room.default_room_timeline_builder().await {
                        Ok(builder) => builder,
                        Err(err) => {
                            error!("error when getting default timeline builder: {err}");
                            continue;
                        }
                    };

                    if let Err(err) = ui_room.init_timeline_with_builder(builder).await {
                        error!("error when creating default timeline: {err}");
                    }

                    // Save the timeline in the cache.
                    let (items, stream) = ui_room.timeline().unwrap().subscribe().await;
                    let items = Arc::new(Mutex::new(items));

                    // Spawn a timeline task that will listen to all the timeline item changes.
                    let i = items.clone();
                    let timeline_task = spawn(async move {
                        pin_mut!(stream);
                        let items = i;
                        while let Some(diff) = stream.next().await {
                            let mut items = items.lock().unwrap();
                            diff.apply(&mut items);
                        }
                    });

                    new_timelines.push((room_id.clone(), Timeline { items, task: timeline_task }));

                    // Save the room list service room in the cache.
                    new_ui_rooms.insert(room_id, ui_room);
                }

                ui_rooms.lock().unwrap().extend(new_ui_rooms);
                timelines.lock().unwrap().extend(new_timelines);
            }
        });

        // This will sync (with encryption) until an error happens or the program is
        // stopped.
        sync_service.start().await;

        Ok(Self {
            sync_service,
            room_list_entries: StatefulList { state: Default::default(), items: rooms },
            client,
            listen_task,
            last_status_message: Default::default(),
            clear_status_message: None,
            ui_rooms,
            details_mode: Default::default(),
            timelines,
            current_room_subscription: None,
        })
    }
}

impl App {
    /// Set the current status message (displayed at the bottom), for a few
    /// seconds.
    fn set_status_message(&mut self, status: String) {
        if let Some(handle) = self.clear_status_message.take() {
            // Cancel the previous task to clear the status message.
            handle.abort();
        }

        *self.last_status_message.lock().unwrap() = Some(status);

        let message = self.last_status_message.clone();
        self.clear_status_message = Some(spawn(async move {
            // Clear the status message in 4 seconds.
            tokio::time::sleep(Duration::from_secs(4)).await;

            *message.lock().unwrap() = None;
        }));
    }

    /// Mark the currently selected room as read.
    async fn mark_as_read(&mut self) -> anyhow::Result<()> {
        if let Some(room) = self
            .room_list_entries
            .state
            .selected()
            .and_then(|selected| {
                self.room_list_entries.items.lock().unwrap().get(selected).cloned()
            })
            .and_then(|entry| entry.as_room_id().map(ToOwned::to_owned))
            .and_then(|room_id| self.ui_rooms.lock().unwrap().get(&room_id).cloned())
        {
            // Mark as read!
            let did = room.timeline().unwrap().mark_as_read(ReceiptType::Read).await?;

            self.set_status_message(format!(
                "did {}send a read receipt!",
                if did { "" } else { "not " }
            ));
        } else {
            self.set_status_message("missing room or nothing to show".to_owned());
        }

        Ok(())
    }

    fn subscribe_to_selected_room(&mut self, selected: usize) {
        // Delete the subscription to the previous room, if any.
        if let Some(room) = self.current_room_subscription.take() {
            room.unsubscribe();
        }

        // Subscribe to the new room.
        if let Some(room) = self
            .room_list_entries
            .items
            .lock()
            .unwrap()
            .get(selected)
            .cloned()
            .and_then(|entry| entry.as_room_id().map(ToOwned::to_owned))
            .and_then(|room_id| self.ui_rooms.lock().unwrap().get(&room_id).cloned())
        {
            room.subscribe(None);
            self.current_room_subscription = Some(room);
        }
    }

    async fn render_loop(&mut self, mut terminal: Terminal<impl Backend>) -> anyhow::Result<()> {
        loop {
            terminal.draw(|f| f.render_widget(&mut *self, f.size()))?;

            if crossterm::event::poll(Duration::from_millis(16))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        use KeyCode::*;
                        match key.code {
                            Char('q') | Esc => return Ok(()),

                            Char('j') | Down => {
                                if let Some(i) = self.room_list_entries.next() {
                                    self.subscribe_to_selected_room(i);
                                }
                            }

                            Char('k') | Up => {
                                if let Some(i) = self.room_list_entries.previous() {
                                    self.subscribe_to_selected_room(i);
                                }
                            }

                            Char('s') => self.sync_service.start().await,
                            Char('S') => self.sync_service.stop().await?,
                            Char('r') => self.details_mode = DetailsMode::ReadReceipts,
                            Char('t') => self.details_mode = DetailsMode::TimelineItems,

                            Char('b') if self.details_mode == DetailsMode::TimelineItems => {}

                            Char('m') if self.details_mode == DetailsMode::ReadReceipts => {
                                self.mark_as_read().await?
                            }

                            _ => {}
                        }
                    }
                }
            }
        }
    }

    async fn run(&mut self, terminal: Terminal<impl Backend>) -> anyhow::Result<()> {
        self.render_loop(terminal).await?;

        // At this point the user has exited the loop, so shut down the application.
        restore_terminal()?;

        println!("Closing sync service...");

        let s = self.sync_service.clone();
        let wait_for_termination = spawn(async move {
            while let Some(state) = s.state().next().await {
                if !matches!(state, sync_service::State::Running) {
                    break;
                }
            }
        });

        self.sync_service.stop().await?;
        self.listen_task.abort();
        for timeline in self.timelines.lock().unwrap().values() {
            timeline.task.abort();
        }
        wait_for_termination.await.unwrap();

        println!("okthxbye!");
        Ok(())
    }
}

impl Widget for &mut App {
    /// Render the whole app.
    fn render(self, area: Rect, buf: &mut Buffer) {
        // Create a space for header, todo list and the footer.
        let vertical =
            Layout::vertical([Constraint::Length(2), Constraint::Min(0), Constraint::Length(2)]);
        let [header_area, rest_area, footer_area] = vertical.areas(area);

        // Create two chunks with equal horizontal screen space. One for the list and
        // the other for the info block.
        let horizontal =
            Layout::horizontal([Constraint::Percentage(50), Constraint::Percentage(50)]);
        let [lhs, rhs] = horizontal.areas(rest_area);

        self.render_title(header_area, buf);
        self.render_left(lhs, buf);
        self.render_right(rhs, buf);
        self.render_footer(footer_area, buf);
    }
}

impl App {
    /// Render the top square (title of the program).
    fn render_title(&self, area: Rect, buf: &mut Buffer) {
        Paragraph::new("Multiverse").bold().centered().render(area, buf);
    }

    /// Renders the left part of the screen, that is, the list of rooms.
    fn render_left(&mut self, area: Rect, buf: &mut Buffer) {
        // We create two blocks, one is for the header (outer) and the other is for list
        // (inner).
        let outer_block = Block::default()
            .borders(Borders::NONE)
            .fg(TEXT_COLOR)
            .bg(HEADER_BG)
            .title("Room list")
            .title_alignment(Alignment::Center);
        let inner_block =
            Block::default().borders(Borders::NONE).fg(TEXT_COLOR).bg(NORMAL_ROW_COLOR);

        // We get the inner area from outer_block. We'll use this area later to render
        // the table.
        let outer_area = area;
        let inner_area = outer_block.inner(outer_area);

        // We can render the header in outer_area.
        outer_block.render(outer_area, buf);

        // Iterate through all elements in the `items` and stylize them.
        let items: Vec<ListItem<'_>> = self
            .room_list_entries
            .items
            .lock()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let bg_color = match i % 2 {
                    0 => NORMAL_ROW_COLOR,
                    _ => ALT_ROW_COLOR,
                };

                let line = if let Some(room) =
                    item.as_room_id().and_then(|room_id| self.client.get_room(room_id))
                {
                    format!("#{i} {}", room.room_id())
                } else {
                    "non-filled room".to_owned()
                };

                let line = Line::styled(line, TEXT_COLOR);
                ListItem::new(line).bg(bg_color)
            })
            .collect();

        // Create a List from all list items and highlight the currently selected one.
        let items = List::new(items)
            .block(inner_block)
            .highlight_style(
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::REVERSED)
                    .fg(SELECTED_STYLE_FG),
            )
            .highlight_symbol(">")
            .highlight_spacing(HighlightSpacing::Always);

        StatefulWidget::render(items, inner_area, buf, &mut self.room_list_entries.state);
    }

    /// Render the right part of the screen, showing the details of the current
    /// view.
    fn render_right(&mut self, area: Rect, buf: &mut Buffer) {
        // Split the block into two parts:
        // - outer_block with the title of the block.
        // - inner_block that will contain the actual details.
        let outer_block = Block::default()
            .borders(Borders::NONE)
            .fg(TEXT_COLOR)
            .bg(HEADER_BG)
            .title("Room view")
            .title_alignment(Alignment::Center);
        let inner_block = Block::default()
            .borders(Borders::NONE)
            .bg(NORMAL_ROW_COLOR)
            .padding(Padding::horizontal(1));

        // This is a similar process to what we did for list. outer_info_area will be
        // used for header inner_info_area will be used for the list info.
        let outer_area = area;
        let inner_area = outer_block.inner(outer_area);

        // We can render the header. Inner area will be rendered later.
        outer_block.render(outer_area, buf);

        // Helper to render some string as a paragraph.
        let render_paragraph = |buf: &mut Buffer, content: String| {
            Paragraph::new(content)
                .block(inner_block.clone())
                .fg(TEXT_COLOR)
                .wrap(Wrap { trim: false })
                .render(inner_area, buf);
        };

        if let Some(room_id) = self
            .room_list_entries
            .state
            .selected()
            .and_then(|i| self.room_list_entries.items.lock().unwrap().get(i).cloned())
            .and_then(|room_entry| room_entry.as_room_id().map(ToOwned::to_owned))
        {
            match self.details_mode {
                DetailsMode::ReadReceipts => {
                    // In read receipts mode, show the read receipts object as computed
                    // by the client.
                    match self.ui_rooms.lock().unwrap().get(&room_id).cloned() {
                        Some(room) => {
                            let receipts = room.read_receipts();
                            render_paragraph(
                                buf,
                                format!(
                                    r#"Read receipts:
- unread: {}
- notifications: {}
- mentions: {}

---

{:?}
"#,
                                    receipts.num_unread,
                                    receipts.num_notifications,
                                    receipts.num_mentions,
                                    receipts
                                ),
                            )
                        }
                        None => render_paragraph(
                            buf,
                            "(room disappeared in the room list service)".to_owned(),
                        ),
                    }
                }

                DetailsMode::TimelineItems => {
                    if !self.render_timeline(&room_id, inner_block.clone(), inner_area, buf) {
                        render_paragraph(buf, "(room's timeline disappeared)".to_owned())
                    }
                }
            }
        } else {
            render_paragraph(buf, "Nothing to see here...".to_owned())
        };
    }

    /// Renders the list of timeline items for the given room.
    fn render_timeline(
        &mut self,
        room_id: &RoomId,
        inner_block: Block<'_>,
        inner_area: Rect,
        buf: &mut Buffer,
    ) -> bool {
        let Some(items) =
            self.timelines.lock().unwrap().get(room_id).map(|timeline| timeline.items.clone())
        else {
            return false;
        };

        let items = items.lock().unwrap();
        let mut content = Vec::new();

        for item in items.iter() {
            match item.kind() {
                TimelineItemKind::Event(ev) => {
                    let sender = ev.sender();

                    match ev.content() {
                        TimelineItemContent::Message(message) => {
                            if let MessageType::Text(text) = message.msgtype() {
                                content.push(format!("{}: {}", sender, text.body))
                            }
                        }

                        TimelineItemContent::RedactedMessage => {
                            content.push(format!("{}: -- redacted --", sender))
                        }
                        TimelineItemContent::UnableToDecrypt(_) => {
                            content.push(format!("{}: (UTD)", sender))
                        }
                        TimelineItemContent::Sticker(_)
                        | TimelineItemContent::MembershipChange(_)
                        | TimelineItemContent::ProfileChange(_)
                        | TimelineItemContent::OtherState(_)
                        | TimelineItemContent::FailedToParseMessageLike { .. }
                        | TimelineItemContent::FailedToParseState { .. }
                        | TimelineItemContent::Poll(_)
                        | TimelineItemContent::CallInvite => {
                            continue;
                        }
                    }
                }

                TimelineItemKind::Virtual(virt) => match virt {
                    VirtualTimelineItem::DayDivider(unix_ts) => {
                        content.push(format!("Date: {unix_ts:?}"));
                    }
                    VirtualTimelineItem::ReadMarker => {
                        content.push("Read marker".to_owned());
                    }
                },
            }
        }

        let list_items = content
            .into_iter()
            .enumerate()
            .map(|(i, line)| {
                let bg_color = match i % 2 {
                    0 => NORMAL_ROW_COLOR,
                    _ => ALT_ROW_COLOR,
                };
                let line = Line::styled(line, TEXT_COLOR);
                ListItem::new(line).bg(bg_color)
            })
            .collect::<Vec<_>>();

        let list = List::new(list_items)
            .block(inner_block)
            .highlight_style(
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .add_modifier(Modifier::REVERSED)
                    .fg(SELECTED_STYLE_FG),
            )
            .highlight_symbol(">")
            .highlight_spacing(HighlightSpacing::Always);

        let mut dummy_list_state = ListState::default();
        StatefulWidget::render(list, inner_area, buf, &mut dummy_list_state);
        true
    }

    /// Render the bottom part of the screen, with a status message if one is
    /// set, or a default help message otherwise.
    fn render_footer(&self, area: Rect, buf: &mut Buffer) {
        let content = if let Some(status_message) = self.last_status_message.lock().unwrap().clone()
        {
            status_message
        } else {
            match self.details_mode {
                DetailsMode::ReadReceipts => {
                    "\nUse ↓↑ to move, s/S to start/stop the sync service, m to mark as read, t to show the timeline.".to_owned()
                }
                DetailsMode::TimelineItems => {
                    "\nUse ↓↑ to move, s/S to start/stop the sync service, r to show read receipts.".to_owned()
                }
            }
        };
        Paragraph::new(content).centered().render(area, buf);
    }
}

impl<T> StatefulList<T> {
    /// Focus the list on the next item, wraps around if needs be.
    ///
    /// Returns the index only if there was a meaningful change.
    fn next(&mut self) -> Option<usize> {
        let num_items = self.items.lock().unwrap().len();

        // If there's no item to select, leave early.
        if num_items == 0 {
            self.state.select(None);
            return None;
        }

        // Otherwise, select the next one or wrap around.
        let prev = self.state.selected();
        let new = prev.map_or(0, |i| if i >= num_items - 1 { 0 } else { i + 1 });

        if prev != Some(new) {
            self.state.select(Some(new));
            Some(new)
        } else {
            None
        }
    }

    /// Focus the list on the previous item, wraps around if needs be.
    ///
    /// Returns the index only if there was a meaningful change.
    fn previous(&mut self) -> Option<usize> {
        let num_items = self.items.lock().unwrap().len();

        // If there's no item to select, leave early.
        if num_items == 0 {
            self.state.select(None);
            return None;
        }

        // Otherwise, select the previous one or wrap around.
        let prev = self.state.selected();
        let new = prev.map_or(0, |i| if i == 0 { num_items - 1 } else { i - 1 });

        if prev != Some(new) {
            self.state.select(Some(new));
            Some(new)
        } else {
            None
        }
    }
}

/// Configure the client so it's ready for sync'ing.
///
/// Will log in or reuse a previous session.
async fn configure_client(server_name: String, config_path: String) -> anyhow::Result<Client> {
    let server_name = ServerName::parse(&server_name)?;

    let config_path = PathBuf::from(config_path);
    let client = Client::builder()
        .store_config(
            StoreConfig::default()
                .crypto_store(
                    SqliteCryptoStore::open(config_path.join("crypto.sqlite"), None).await?,
                )
                .state_store(SqliteStateStore::open(config_path.join("state.sqlite"), None).await?),
        )
        .server_name(&server_name)
        .with_encryption_settings(EncryptionSettings {
            auto_enable_cross_signing: true,
            backup_download_strategy: BackupDownloadStrategy::AfterDecryptionFailure,
            auto_enable_backups: true,
        })
        .build()
        .await?;

    // Try reading a session, otherwise create a new one.
    let session_path = config_path.join("session.json");
    if let Ok(serialized) = std::fs::read_to_string(&session_path) {
        let session: MatrixSession = serde_json::from_str(&serialized)?;
        client.restore_session(session).await?;
        println!("restored session");
    } else {
        login_with_password(&client).await?;
        println!("new login");

        // Immediately save the session to disk.
        if let Some(session) = client.session() {
            let AuthSession::Matrix(session) = session else { panic!("unexpected oidc session") };
            let serialized = serde_json::to_string(&session)?;
            std::fs::write(session_path, serialized)?;
            println!("saved session");
        }
    }

    Ok(client)
}

/// Asks the user of a username and password, and try to login using the matrix
/// auth with those.
async fn login_with_password(client: &Client) -> anyhow::Result<()> {
    println!("Logging in with username and password…");

    loop {
        print!("\nUsername: ");
        stdout().flush().expect("Unable to write to stdout");
        let mut username = String::new();
        io::stdin().read_line(&mut username).expect("Unable to read user input");
        username = username.trim().to_owned();

        print!("Password: ");
        stdout().flush().expect("Unable to write to stdout");
        let mut password = String::new();
        io::stdin().read_line(&mut password).expect("Unable to read user input");
        password = password.trim().to_owned();

        match client.matrix_auth().login_username(&username, &password).await {
            Ok(_) => {
                println!("Logged in as {username}");
                break;
            }
            Err(error) => {
                println!("Error logging in: {error}");
                println!("Please try again\n");
            }
        }
    }

    Ok(())
}
