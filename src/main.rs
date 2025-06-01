use chrono::Local;
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::{Backend, CrosstermBackend},
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Paragraph, Row, Table, Wrap},
    Frame, Terminal,
};
use serde_json::Value;
use std::{
    collections::HashMap,
    io::{self},
    sync::mpsc::{self, Receiver, Sender},
    thread,
    time::Duration,
};
use tungstenite::{connect, Message};
use rusqlite::{params, Connection, Result as RusqliteResult};

const DB_PATH: &str = "trades.db";

#[derive(Clone, Debug)]
struct Trade {
    timestamp: String,
    trade_type_val: String,
    action: String,
    username: String,
    amount: f64,
    coin_symbol: String,
    total_value: f64,
    price: f64,
}

#[derive(PartialEq)]
enum InputMode {
    Normal,
    Editing,
}

struct App {
    search_input: String,
    active_search_symbol: Option<String>,
    all_trades: Vec<Trade>,
    user_balances: HashMap<String, HashMap<String, f64>>,
    scroll_offset: usize,
    trade_type_filter: Option<String>,
    input_mode: InputMode,
    cursor_position: usize,
}

impl App {
    fn new(initial_trades: Vec<Trade>) -> App {
        App {
            search_input: String::new(),
            active_search_symbol: None,
            all_trades: initial_trades,
            user_balances: HashMap::new(),
            scroll_offset: 0,
            trade_type_filter: None,
            input_mode: InputMode::Normal,
            cursor_position: 0,
        }
    }

    fn add_trade(&mut self, trade: Trade, conn: &Connection) {
        if insert_trade_db(conn, &trade).is_err() {
            eprintln!("Failed to save trade to DB: {:?}", trade);
        }

        let user_coin_balances = self
            .user_balances
            .entry(trade.username.clone())
            .or_insert_with(HashMap::new);
        let balance = user_coin_balances
            .entry(trade.coin_symbol.clone())
            .or_insert(0.0);

        if trade.action == "BUY" {
            *balance += trade.amount;
        } else if trade.action == "SELL" {
            *balance -= trade.amount;
        }
        
        self.all_trades.insert(0, trade);
    }

    fn recalculate_balances_from_trades(&mut self) {
        self.user_balances.clear();
        for trade in self.all_trades.iter().rev() {
            let user_coin_balances = self
                .user_balances
                .entry(trade.username.clone())
                .or_insert_with(HashMap::new);
            let balance = user_coin_balances
                .entry(trade.coin_symbol.clone())
                .or_insert(0.0);

            if trade.action == "BUY" {
                *balance += trade.amount;
            } else if trade.action == "SELL" {
                *balance -= trade.amount;
            }
        }
    }


    fn get_visible_trades(&self) -> Vec<Trade> { 
        let trades_after_type_filter: Vec<Trade> = match self.trade_type_filter.as_deref() {
            None => {
                self.all_trades.iter().cloned().collect()
            }
            Some(specific_filter_type) => {
                self.all_trades.iter()
                    .filter(|t| t.trade_type_val.to_lowercase() == specific_filter_type.to_lowercase())
                    .cloned()
                    .collect()
            }
        };

        if let Some(symbol) = &self.active_search_symbol {
            trades_after_type_filter
                .into_iter() 
                .filter(|t| t.coin_symbol.to_uppercase() == *symbol)
                .collect()
        } else {
            trades_after_type_filter
        }
    }
    
    fn scroll_up(&mut self) {
        if self.scroll_offset > 0 {
            self.scroll_offset -= 1;
        }
    }

    fn scroll_down(&mut self, num_visible_items: usize) {
        let total_items = self.get_visible_trades().len();
        if total_items > 0 && self.scroll_offset < total_items.saturating_sub(1) {
            if total_items > num_visible_items && self.scroll_offset < total_items - num_visible_items {
                self.scroll_offset += 1;
            } else if total_items <= num_visible_items && self.scroll_offset < total_items -1 {
                 self.scroll_offset += 1;
            } else if total_items > num_visible_items && self.scroll_offset >= total_items - num_visible_items {
                self.scroll_offset = total_items - num_visible_items;
            }
        }
    }

    fn toggle_trade_type_filter(&mut self) {
        match self.trade_type_filter.as_deref() {
            Some("live-trade") => { 
                self.trade_type_filter = Some("all-trades".to_string()); 
            }
            Some("all-trades") => { 
                self.trade_type_filter = Some("live-trade".to_string());
            }
            None => {
                self.trade_type_filter = Some("all-trades".to_string());
            }
            _ => { 
                self.trade_type_filter = Some("live-trade".to_string());
            }
        }
        self.scroll_offset = 0;
    }

    fn move_cursor_left(&mut self) {
        if self.cursor_position > 0 {
            self.cursor_position -= 1;
        }
    }

    fn move_cursor_right(&mut self) {
        if self.cursor_position < self.search_input.len() {
            self.cursor_position += 1;
        }
    }

    fn enter_char(&mut self, new_char: char) {
        self.search_input.insert(self.cursor_position, new_char);
        self.move_cursor_right();
    }

    fn delete_char(&mut self) {
        if self.cursor_position > 0 && !self.search_input.is_empty() {
            self.search_input.remove(self.cursor_position - 1);
            self.move_cursor_left();
        }
    }

    fn submit_search(&mut self) {
        if self.search_input.is_empty() {
            self.active_search_symbol = None;
        } else {
            self.active_search_symbol = Some(self.search_input.to_uppercase().clone());
        }
        self.scroll_offset = 0; 
    }
}

fn init_db(conn: &Connection) -> RusqliteResult<()> {
    conn.execute(
        "CREATE TABLE IF NOT EXISTS trades (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp TEXT NOT NULL,
            trade_type_val TEXT NOT NULL,
            action TEXT NOT NULL,
            username TEXT NOT NULL,
            amount REAL NOT NULL,
            coin_symbol TEXT NOT NULL,
            total_value REAL NOT NULL,
            price REAL NOT NULL
        )",
        [],
    )?;
    Ok(())
}

fn insert_trade_db(conn: &Connection, trade: &Trade) -> RusqliteResult<usize> {
    conn.execute(
        "INSERT INTO trades (timestamp, trade_type_val, action, username, amount, coin_symbol, total_value, price)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            trade.timestamp,
            trade.trade_type_val,
            trade.action,
            trade.username,
            trade.amount,
            trade.coin_symbol,
            trade.total_value,
            trade.price
        ],
    )
}

fn load_trades_from_db(conn: &Connection) -> RusqliteResult<Vec<Trade>> {
    let mut stmt = conn.prepare("SELECT timestamp, trade_type_val, action, username, amount, coin_symbol, total_value, price FROM trades ORDER BY id DESC")?;
    let trade_iter = stmt.query_map([], |row| {
        Ok(Trade {
            timestamp: row.get(0)?,
            trade_type_val: row.get(1)?,
            action: row.get(2)?,
            username: row.get(3)?,
            amount: row.get(4)?,
            coin_symbol: row.get(5)?,
            total_value: row.get(6)?,
            price: row.get(7)?,
        })
    })?;

    let mut trades = Vec::new();
    for trade in trade_iter {
        trades.push(trade?);
    }
    Ok(trades)
}


fn main() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::open(DB_PATH)?;
    init_db(&conn)?;
    let initial_trades = load_trades_from_db(&conn).unwrap_or_else(|e| {
        eprintln!("Failed to load trades from DB: {}. Starting with empty list.", e);
        Vec::new()
    });

    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let (tx, rx): (Sender<Trade>, Receiver<Trade>) = mpsc::channel();

    thread::spawn(move || {
        let (mut socket, _response) =
            connect("ws://ws.rugplay.com/api/").expect("Can't connect to WebSocket");

        socket
            .send(Message::Text(
                "{\"type\":\"subscribe\",\"channel\":\"trades:all\"}".into(),
            ))
            .unwrap();
        socket
            .send(Message::Text(
                "{\"type\":\"set_coin\",\"coinSymbol\":\"@global\"}".into(),
            ))
            .unwrap();

        loop {
            match socket.read() {
                Ok(msg) => {
                    if msg.is_text() || msg.is_binary() {
                        let message_str = msg.to_string();
                        let v: Value = match serde_json::from_str(&message_str) {
                            Ok(val) => val,
                            Err(_) => continue,
                        };

                        let trade_type_val = v["type"].as_str().unwrap_or_default().to_string();
                        if trade_type_val == "ping" {
                            continue;
                        }

                        if v["data"].is_object() {
                            let data = &v["data"];
                            let action = data["type"].as_str().unwrap_or_default().to_string();
                            let username = data["username"].as_str().unwrap_or_default().to_string();
                            let amount = data["amount"].as_f64().unwrap_or_default();
                            let coin_symbol = data["coinSymbol"].as_str().unwrap_or_default().to_string();
                            let total_value = data["totalValue"].as_f64().unwrap_or_default();
                            let price = data["price"].as_f64().unwrap_or_default();
                            let timestamp = Local::now().format("%H:%M:%S").to_string();

                            let trade = Trade {
                                timestamp,
                                trade_type_val,
                                action,
                                username,
                                amount,
                                coin_symbol,
                                total_value,
                                price,
                            };

                            if tx.send(trade).is_err() {
                                break; 
                            }
                        }
                    }
                }
                Err(_e) => {
                    break;
                }
            }
        }
    });

    let mut app = App::new(initial_trades);
    app.recalculate_balances_from_trades();

    run_app(&mut terminal, app, rx, &conn)?;

    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    Ok(())
}

fn run_app<B: Backend>(
    terminal: &mut Terminal<B>,
    mut app: App,
    rx: Receiver<Trade>,
    conn: &Connection,
) -> io::Result<()> {
    loop {
        match rx.try_recv() {
            Ok(trade) => {
                let was_at_top = app.scroll_offset == 0;
                
                app.add_trade(trade, conn); 

                if !was_at_top && app.input_mode == InputMode::Normal {
                    app.scroll_offset += 1;
                }
            }
            Err(mpsc::TryRecvError::Empty) => {}
            Err(mpsc::TryRecvError::Disconnected) => {
                break; 
            }
        }

        terminal.draw(|f| ui(f, &mut app))?;

        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                match app.input_mode {
                    InputMode::Normal => match key.code {
                        KeyCode::Char('q') => return Ok(()),
                        KeyCode::Char('t') => app.toggle_trade_type_filter(),
                        KeyCode::Char('e') | KeyCode::Char('/') => {
                            app.input_mode = InputMode::Editing;
                        }
                        KeyCode::Enter => app.submit_search(),
                        KeyCode::Up => app.scroll_up(),
                        KeyCode::Down => {
                            let visible_trade_area_height = terminal.size().map_or(0, |s| if s.height > 5 {s.height - 5} else {0}) as usize;
                            app.scroll_down(visible_trade_area_height);
                        }
                        KeyCode::PageUp => {
                            for _ in 0..10 { app.scroll_up(); }
                        }
                        KeyCode::PageDown => {
                            let visible_trade_area_height = terminal.size().map_or(0, |s| if s.height > 5 {s.height - 5} else {0}) as usize;
                            for _ in 0..10 { app.scroll_down(visible_trade_area_height); }
                        }
                        _ => {}
                    },
                    InputMode::Editing => match key.code {
                        KeyCode::Enter => {
                            app.submit_search();
                            app.input_mode = InputMode::Normal;
                        }
                        KeyCode::Char(c) => {
                            app.enter_char(c);
                        }
                        KeyCode::Backspace => {
                            app.delete_char();
                        }
                        KeyCode::Left => {
                            app.move_cursor_left();
                        }
                        KeyCode::Right => {
                            app.move_cursor_right();
                        }
                        KeyCode::Esc => {
                            app.input_mode = InputMode::Normal;
                        }
                        KeyCode::Home => {
                            app.cursor_position = 0;
                        }
                        KeyCode::End => {
                            app.cursor_position = app.search_input.len();
                        }
                        _ => {}
                    },
                }
            }
        }
    }
    Ok(())
}

fn ui(f: &mut Frame, app: &mut App) {
    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .margin(1)
        .constraints(
            [
                Constraint::Length(3), 
                Constraint::Min(0),    
            ]
            .as_ref(),
        )
        .split(f.size());

    let current_search_mode_hint = match app.input_mode {
        InputMode::Normal => "(Press 'e' or '/' to edit, Enter to search)",
        InputMode::Editing => "(ESC to cancel, Enter to search)",
    };
    
    let search_title_base = if app.active_search_symbol.is_some() {
        format!("Searching: {}", app.active_search_symbol.as_ref().unwrap())
    } else {
        "Search Symbol".to_string()
    };
    let search_title = format!("{} {} (q:quit, t:type)", search_title_base, current_search_mode_hint);

    let input_block = Block::default().title(search_title).borders(Borders::ALL);
    let input_paragraph = Paragraph::new(app.search_input.as_str())
        .block(input_block)
        .wrap(Wrap { trim: true });
    f.render_widget(input_paragraph, main_chunks[0]);

    match app.input_mode {
        InputMode::Editing => {
            f.set_cursor(
                main_chunks[0].x + app.cursor_position as u16 + 1,
                main_chunks[0].y + 1,
            )
        }
        InputMode::Normal => {
        }
    }
    
    let content_area = main_chunks[1];
    let visible_trades = app.get_visible_trades(); 
    
    let mut trades_display_block_title = if let Some(symbol) = &app.active_search_symbol {
        format!("Trades for {}", symbol)
    } else {
        "Trades".to_string() 
    };

    let type_filter_display_name = match app.trade_type_filter.as_deref() {
        Some("all-trades") => "all-trades".to_string(),
        Some(filter_type) => filter_type.to_string(),
        None => "All".to_string(),
    };
    trades_display_block_title = format!("{} (Type: {})", trades_display_block_title, type_filter_display_name);


    if let Some(symbol) = &app.active_search_symbol {
        let side_by_side_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(65), Constraint::Percentage(35)].as_ref())
            .split(content_area);

        draw_trades_table(f, &mut app.scroll_offset, &visible_trades, side_by_side_chunks[0], &trades_display_block_title);

        let balances_block = Block::default()
            .title(format!("Balances for {}", symbol))
            .borders(Borders::ALL);
        
        let mut user_coin_balances: Vec<(String, f64)> = app
            .user_balances
            .iter()
            .filter_map(|(username, coin_map)| {
                coin_map.get(symbol).map(|balance| (username.clone(), *balance))
            })
            .filter(|(_, balance)| *balance != 0.0) 
            .collect();
        
        user_coin_balances.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        let header_cells = ["User", "Balance"]
            .iter()
            .map(|h| Cell::from(*h).style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
        let header = Row::new(header_cells).height(1).bottom_margin(1);

        let rows = user_coin_balances.iter().map(|(username, balance)| {
            Row::new(vec![
                Cell::from(username.as_str()),
                Cell::from(format!("{:.2}", balance)),
            ])
        });

        let balance_table = Table::new(
                rows,
                [Constraint::Percentage(60), Constraint::Percentage(40)]
            )
            .header(header)
            .block(balances_block)
            .widths([Constraint::Percentage(70), Constraint::Percentage(30)]);

        f.render_widget(balance_table, side_by_side_chunks[1]);

    } else {
        draw_trades_table(f, &mut app.scroll_offset, &visible_trades, content_area, &trades_display_block_title);
    }
}

fn draw_trades_table(f: &mut Frame, scroll_offset: &mut usize, trades_to_display: &[Trade], area: Rect, title: &str) {
    let trades_block = Block::default().title(title.to_string()).borders(Borders::ALL);

    let header_cells = [
        "Time", "Type", "Action", "User", "Amount", "Coin", "Total USD", "Price USD",
    ]
    .iter()
    .map(|h| Cell::from(*h).style(Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
    let header = Row::new(header_cells).height(1).bottom_margin(1);

    let rows: Vec<Row> = trades_to_display.iter().map(|trade| {
        let action_color = if trade.action == "BUY" {
            Color::Green
        } else if trade.action == "SELL" {
            Color::Red
        } else {
            Color::Gray
        };

        let row_style = match trade.total_value {
            v if v >= 10000.0 => Style::default().fg(Color::LightRed).add_modifier(Modifier::BOLD),
            v if v >= 1000.0 => Style::default().fg(Color::Magenta),
            v if v >= 100.0 => Style::default().fg(Color::Yellow),
            v if v >= 10.0 => Style::default().fg(Color::Cyan),
            _ => Style::default(),
        };

        Row::new(vec![
            Cell::from(trade.timestamp.as_str()),
            Cell::from(trade.trade_type_val.as_str()),
            Cell::from(Span::styled(trade.action.as_str(), Style::default().fg(action_color))),
            Cell::from(trade.username.as_str()),
            Cell::from(format!("{:.2}", trade.amount)),
            Cell::from(trade.coin_symbol.as_str()),
            Cell::from(format!("{:.2}", trade.total_value)),
            Cell::from(format!("{:.8}", trade.price)),
        ])
        .style(row_style)
    }).collect();

    let visible_row_count = if area.height > 3 { area.height as usize - 3 } else { 0 };

    if trades_to_display.is_empty() {
        *scroll_offset = 0;
    } else if *scroll_offset >= trades_to_display.len() {
        *scroll_offset = trades_to_display.len().saturating_sub(1);
    }
    if trades_to_display.len() > visible_row_count && *scroll_offset > trades_to_display.len() - visible_row_count {
        *scroll_offset = trades_to_display.len() - visible_row_count;
    }
    
    let start_index = *scroll_offset;
    
    let visible_rows_slice = if !rows.is_empty() && start_index < rows.len() {
        let end_idx = (start_index + visible_row_count).min(rows.len());
        &rows[start_index..end_idx]
    } else {
        &[]
    };
    
    let column_widths = [
        Constraint::Length(8),
        Constraint::Length(10),
        Constraint::Length(6),
        Constraint::Length(15),
        Constraint::Length(10),
        Constraint::Length(8),
        Constraint::Length(12),
        Constraint::Length(14),
    ];

    let table = Table::new(visible_rows_slice.to_vec(), column_widths.clone())
        .header(header)
        .block(trades_block)
        .widths(&column_widths);

    f.render_widget(table, area);

    let total_rows_to_display = trades_to_display.len();
    if total_rows_to_display > visible_row_count {
        let scrollbar_area = area.inner(&ratatui::layout::Margin { vertical: 1, horizontal: 0 });
        if scrollbar_area.width > 0 && scrollbar_area.height > 0 {
            let content_height = total_rows_to_display;
            let view_height = visible_row_count;

            let scrollbar_thumb_height = ((view_height as f32 / content_height as f32) * scrollbar_area.height as f32).max(1.0) as u16;
            let scrollbar_track_height = scrollbar_area.height;
            
            let scrollable_content_range = content_height.saturating_sub(view_height);
            let scrollbar_movement_range = scrollbar_track_height.saturating_sub(scrollbar_thumb_height);
            
            let scrollbar_pos = if scrollable_content_range > 0 {
                ((*scroll_offset as f32 / scrollable_content_range as f32) * scrollbar_movement_range as f32) as u16
            } else {
                0
            };
            let scrollbar_pos = scrollbar_pos.min(scrollbar_track_height.saturating_sub(scrollbar_thumb_height));

            for y_offset in 0..scrollbar_track_height {
                let char_to_draw = if y_offset >= scrollbar_pos && y_offset < scrollbar_pos + scrollbar_thumb_height { '█' } else { '░' };
                if area.right() > area.left() {
                    f.buffer_mut().set_string(
                        area.right() - 1, 
                        scrollbar_area.top() + y_offset,
                        char_to_draw.to_string(),
                        Style::default().fg(Color::DarkGray)
                    );
                }
            }
        }
    }
}
