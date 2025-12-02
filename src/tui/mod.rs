use std::io::{self, Stdout};
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use aws_sdk_s3::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_s3::operation::restore_object::RestoreObjectError;

use crate::app::{ActivePane, App, AppMode, MaskEditorField, PendingAction, StorageIntent};
use crate::aws::S3Service;
use crate::mask::ObjectMask;
use crate::models::{RestoreState, StorageClassTier};
use crate::policy::{MigrationPolicy, PolicyStore};

pub async fn run(app: &mut App, s3: &S3Service, policy_store: &mut PolicyStore) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    terminal.hide_cursor()?;

    app.push_status("Loading buckets…");
    if let Err(err) = refresh_buckets(app, s3).await {
        app.push_status(&format!("Failed to load buckets: {err:#}"));
    }

    let result = event_loop(&mut terminal, app, s3, policy_store).await;
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    result
}

async fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    app: &mut App,
    s3: &S3Service,
    policy_store: &mut PolicyStore,
) -> Result<()> {
    loop {
        terminal.draw(|frame| draw(frame, app))?;
        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(key) => {
                    if handle_key_event(key, app, s3, policy_store).await? {
                        break;
                    }
                }
                Event::Resize(_, _) => continue,
                _ => continue,
            }
        }
    }
    Ok(())
}

async fn handle_key_event(
    key: KeyEvent,
    app: &mut App,
    s3: &S3Service,
    policy_store: &mut PolicyStore,
) -> Result<bool> {
    if key.kind != KeyEventKind::Press {
        return Ok(false);
    }

    if matches!(key.code, KeyCode::Char('c')) && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Ok(true);
    }

    match app.mode {
        AppMode::ShowingHelp => {
            if matches!(key.code, KeyCode::Esc | KeyCode::Enter | KeyCode::Char('?')) {
                app.set_mode(AppMode::Browsing);
            }
            return Ok(false);
        }
        AppMode::ViewingLog => {
            if matches!(
                key.code,
                KeyCode::Esc | KeyCode::Enter | KeyCode::Char('l') | KeyCode::Char('L')
            ) {
                app.set_mode(AppMode::Browsing);
            }
            return Ok(false);
        }
        AppMode::EditingMask => {
            handle_mask_editor_keys(key, app);
            return Ok(false);
        }
        AppMode::SelectingStorageClass => {
            handle_storage_class_selector(key, app);
            return Ok(false);
        }
        AppMode::Confirming => {
            handle_confirmation_keys(key, app, s3, policy_store).await?;
            return Ok(false);
        }
        AppMode::Browsing => {}
    }

    match key.code {
        KeyCode::Char('q') => return Ok(true),
        KeyCode::Tab => {
            app.next_pane();
        }
        KeyCode::BackTab => {
            app.previous_pane();
        }
        KeyCode::Up => move_selection(app, -1),
        KeyCode::Down => move_selection(app, 1),
        KeyCode::PageUp => move_selection(app, -5),
        KeyCode::PageDown => move_selection(app, 5),
        KeyCode::Home => jump_selection(app, true),
        KeyCode::End => jump_selection(app, false),
        KeyCode::Char('m') => {
            app.set_mode(AppMode::EditingMask);
            app.focus_mask_field(MaskEditorField::Pattern);
            app.push_status(
                "Mask editor active – Tab moves between fields, arrows/space adjust options, Enter applies",
            );
        }
        KeyCode::Char('f') => {
            app.push_status("Refreshing buckets…");
            if let Err(err) = refresh_buckets(app, s3).await {
                app.push_status(&format!("Bucket refresh failed: {err:#}"));
            }
        }
        KeyCode::Char('i') => {
            if let Err(err) = refresh_selected_object(app, s3).await {
                app.push_status(&format!("Inspect failed: {err:#}"));
            }
        }
        KeyCode::Enter => {
            if app.active_pane == ActivePane::Buckets {
                load_objects_for_selection(app, s3).await?;
            }
        }
        KeyCode::Char('s') => {
            if let Err(err) = begin_storage_selection(app, StorageIntent::Transition) {
                app.push_status(&format!("Storage selection unavailable: {err:#}"));
            }
        }
        KeyCode::Char('r') => {
            if let Err(err) = initiate_restore_flow(app) {
                app.push_status(&format!("Cannot request restore: {err:#}"));
            }
        }
        KeyCode::Char('p') => {
            if let Err(err) = begin_storage_selection(app, StorageIntent::SavePolicy) {
                app.push_status(&format!("Cannot save policy: {err:#}"));
            } else {
                app.push_status("Select target storage class for policy");
            }
        }
        KeyCode::Char('?') => {
            app.set_mode(AppMode::ShowingHelp);
        }
        KeyCode::Char('l') | KeyCode::Char('L') => {
            if matches!(app.mode, AppMode::ViewingLog) {
                app.set_mode(AppMode::Browsing);
            } else {
                app.set_mode(AppMode::ViewingLog);
            }
        }
        KeyCode::Esc => {
            if app.active_mask.is_some() {
                app.apply_mask(None);
            }
        }
        _ => {}
    }

    Ok(false)
}

async fn handle_confirmation_keys(
    key: KeyEvent,
    app: &mut App,
    s3: &S3Service,
    policy_store: &mut PolicyStore,
) -> Result<()> {
    match key.code {
        KeyCode::Esc | KeyCode::Char('n') => {
            app.pending_action = None;
            app.set_mode(AppMode::Browsing);
            app.push_status("Cancelled");
        }
        KeyCode::Enter | KeyCode::Char('y') => {
            if let Some(action) = app.pending_action.take() {
                match action {
                    PendingAction::Transition {
                        target_class,
                        restore_first,
                    } => {
                        execute_transition(app, s3, target_class, restore_first).await?;
                    }
                    PendingAction::Restore { days } => {
                        execute_restore(app, s3, days).await?;
                    }
                    PendingAction::SavePolicy { target_class } => {
                        save_policy(app, policy_store, target_class)?;
                    }
                }
            }
            app.set_mode(AppMode::Browsing);
        }
        KeyCode::Char('o') => {
            let toggle_state = app.pending_action.as_mut().and_then(|action| match action {
                PendingAction::Transition { restore_first, .. } => {
                    *restore_first = !*restore_first;
                    Some(*restore_first)
                }
                _ => None,
            });
            if let Some(state) = toggle_state {
                app.push_status(if state {
                    "Will request restore before transition"
                } else {
                    "Restore before transition disabled"
                });
            }
        }
        _ => {}
    }
    Ok(())
}

fn handle_mask_editor_keys(key: KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Esc => {
            app.set_mode(AppMode::Browsing);
            app.push_status("Mask edit cancelled");
        }
        KeyCode::Enter => {
            if app.mask_draft.pattern.is_empty() {
                app.push_status("Mask pattern cannot be empty");
                return;
            }
            let mask = ObjectMask {
                name: app.mask_draft.name.clone(),
                pattern: app.mask_draft.pattern.clone(),
                kind: app.mask_draft.kind.clone(),
                case_sensitive: app.mask_draft.case_sensitive,
            };
            app.apply_mask(Some(mask));
            app.set_mode(AppMode::Browsing);
        }
        KeyCode::Tab => {
            app.next_mask_field();
        }
        KeyCode::BackTab => {
            app.previous_mask_field();
        }
        KeyCode::Backspace => match app.mask_field {
            MaskEditorField::Name => {
                app.mask_draft.name.pop();
            }
            MaskEditorField::Pattern => {
                app.mask_draft.pattern.pop();
            }
            _ => {}
        },
        KeyCode::Left => {
            if matches!(app.mask_field, MaskEditorField::Mode) {
                app.cycle_mask_kind_backwards();
            }
        }
        KeyCode::Right => {
            if matches!(app.mask_field, MaskEditorField::Mode) {
                app.cycle_mask_kind();
            }
        }
        KeyCode::Char(' ') => match app.mask_field {
            MaskEditorField::Mode => app.cycle_mask_kind(),
            MaskEditorField::Case => app.toggle_mask_case(),
            MaskEditorField::Name => app.mask_draft.name.push(' '),
            MaskEditorField::Pattern => app.mask_draft.pattern.push(' '),
        },
        KeyCode::Char('c') => {
            app.toggle_mask_case();
            app.focus_mask_field(MaskEditorField::Case);
        }
        KeyCode::Char(ch) => match app.mask_field {
            MaskEditorField::Name => app.mask_draft.name.push(ch),
            MaskEditorField::Pattern => app.mask_draft.pattern.push(ch),
            MaskEditorField::Mode => {}
            MaskEditorField::Case => {}
        },
        _ => {}
    }
}

fn handle_storage_class_selector(key: KeyEvent, app: &mut App) {
    match key.code {
        KeyCode::Esc => {
            app.set_mode(AppMode::Browsing);
        }
        KeyCode::Up => {
            if app.storage_class_cursor > 0 {
                app.storage_class_cursor -= 1;
            }
        }
        KeyCode::Down => {
            if app.storage_class_cursor + 1 < StorageClassTier::selectable().len() {
                app.storage_class_cursor += 1;
            }
        }
        KeyCode::Enter => {
            if let Some(selected) = StorageClassTier::selectable().get(app.storage_class_cursor) {
                match app.storage_intent {
                    StorageIntent::Transition => {
                        app.pending_action = Some(PendingAction::Transition {
                            target_class: selected.clone(),
                            restore_first: false,
                        });
                        app.set_mode(AppMode::Confirming);
                        app.push_status(&format!(
                            "Confirm transition to {} (press Enter to confirm)",
                            selected.label()
                        ));
                    }
                    StorageIntent::SavePolicy => {
                        app.pending_action = Some(PendingAction::SavePolicy {
                            target_class: selected.clone(),
                        });
                        app.set_mode(AppMode::Confirming);
                        app.push_status("Confirm saving policy");
                    }
                }
            }
        }
        _ => {}
    }
}

fn begin_storage_selection(app: &mut App, intent: StorageIntent) -> Result<()> {
    if app.selected_bucket_name().is_none() {
        anyhow::bail!("Select a bucket first");
    }
    match intent {
        StorageIntent::Transition => {
            if target_count(app) == 0 {
                anyhow::bail!("Select at least one object (mask or row)");
            }
        }
        StorageIntent::SavePolicy => {
            if app.active_mask.is_none() {
                anyhow::bail!("Apply a mask before saving a policy");
            }
        }
    }
    app.storage_intent = intent;
    app.storage_class_cursor = 0;
    app.set_mode(AppMode::SelectingStorageClass);
    Ok(())
}

fn initiate_restore_flow(app: &mut App) -> Result<()> {
    if app.selected_bucket_name().is_none() || target_count(app) == 0 {
        anyhow::bail!("Select objects to restore first");
    }
    app.pending_action = Some(PendingAction::Restore { days: 7 });
    app.set_mode(AppMode::Confirming);
    app.push_status("Confirm restore request (Enter to proceed, Esc to cancel)");
    Ok(())
}

async fn execute_transition(
    app: &mut App,
    s3: &S3Service,
    target_class: StorageClassTier,
    restore_first: bool,
) -> Result<()> {
    let bucket = app
        .selected_bucket_name()
        .context("Select a bucket before transitioning")?
        .to_string();
    let keys = target_keys(app);
    if keys.is_empty() {
        app.push_status("No objects selected for transition");
        return Ok(());
    }
    for key in keys {
        if restore_first {
            if let Err(err) = s3.request_restore(&bucket, &key, 7).await {
                let detail = describe_restore_error(&err);
                app.push_status(&format!("Restore failed for {key}: {detail}"));
                continue;
            }
        }
        match s3
            .transition_storage_class(&bucket, &key, target_class.clone())
            .await
        {
            Ok(_) => app.push_status(&format!("Transitioned {key} to {}", target_class.label())),
            Err(err) => app.push_status(&format!("Transition failed for {key}: {err:#}")),
        }
    }
    load_objects_for_selection(app, s3).await?;
    Ok(())
}

async fn execute_restore(app: &mut App, s3: &S3Service, days: i32) -> Result<()> {
    let bucket = app
        .selected_bucket_name()
        .context("Select a bucket before restoring")?
        .to_string();
    for key in target_keys(app) {
        match s3.request_restore(&bucket, &key, days).await {
            Ok(_) => app.push_status(&format!("Restore requested for {key}")),
            Err(err) => {
                let detail = describe_restore_error(&err);
                app.push_status(&format!("Restore failed for {key}: {detail}"));
            }
        }
    }
    Ok(())
}

fn save_policy(
    app: &mut App,
    store: &mut PolicyStore,
    target_class: StorageClassTier,
) -> Result<()> {
    let bucket = app
        .selected_bucket_name()
        .context("Select a bucket before saving policy")?
        .to_string();
    let mask = app
        .active_mask
        .clone()
        .context("Apply a mask before saving policy")?;
    let policy = MigrationPolicy::new(bucket, mask, target_class, false, None);
    store.add(policy.clone())?;
    app.policies = store.policies.clone();
    app.push_status("Policy saved");
    Ok(())
}

async fn refresh_buckets(app: &mut App, s3: &S3Service) -> Result<()> {
    let buckets = s3.list_buckets().await?;
    app.set_buckets(buckets);
    Ok(())
}

async fn refresh_selected_object(app: &mut App, s3: &S3Service) -> Result<()> {
    let bucket = app
        .selected_bucket_name()
        .context("Select a bucket first")?
        .to_string();
    let key = app
        .selected_object()
        .map(|obj| obj.key.clone())
        .context("Select an object to inspect")?;
    let refreshed = s3.refresh_object(&bucket, &key).await?;
    if let Some(existing) = app.objects.iter_mut().find(|o| o.key == key) {
        *existing = refreshed.clone();
    }
    if let Some(mask) = &app.active_mask {
        app.filtered_objects = app
            .objects
            .iter()
            .cloned()
            .filter(|obj| mask.matches(&obj.key))
            .collect();
    }
    app.push_status("Object metadata refreshed");
    Ok(())
}

async fn load_objects_for_selection(app: &mut App, s3: &S3Service) -> Result<()> {
    if let Some(bucket) = app.selected_bucket_name().map(|b| b.to_string()) {
        let mut objects = s3.list_objects(&bucket, None).await?;
        objects.sort_by(|a, b| a.key.cmp(&b.key));
        app.set_objects(objects);
        app.apply_mask(app.active_mask.clone());
        app.push_status(&format!("Loaded objects for bucket {}", bucket));
    }
    Ok(())
}

fn move_selection(app: &mut App, delta: isize) {
    match app.active_pane {
        ActivePane::Buckets => {
            if app.buckets.is_empty() {
                return;
            }
            let len = app.buckets.len() as isize;
            let mut idx = app.selected_bucket as isize + delta;
            if idx < 0 {
                idx = 0;
            }
            if idx >= len {
                idx = len - 1;
            }
            app.selected_bucket = idx as usize;
        }
        ActivePane::Objects => {
            let len = app.active_objects().len();
            if len == 0 {
                return;
            }
            let len = len as isize;
            let mut idx = app.selected_object as isize + delta;
            if idx < 0 {
                idx = 0;
            }
            if idx >= len {
                idx = len - 1;
            }
            app.selected_object = idx as usize;
        }
        ActivePane::MaskEditor | ActivePane::Policies => {}
    }
}

fn jump_selection(app: &mut App, start: bool) {
    match app.active_pane {
        ActivePane::Buckets => {
            if !app.buckets.is_empty() {
                app.selected_bucket = if start { 0 } else { app.buckets.len() - 1 };
            }
        }
        ActivePane::Objects => {
            if !app.active_objects().is_empty() {
                app.selected_object = if start {
                    0
                } else {
                    app.active_objects().len() - 1
                };
            }
        }
        _ => {}
    }
}

fn target_count(app: &App) -> usize {
    if app.active_mask.is_some() {
        app.filtered_objects.len()
    } else if app.selected_object < app.objects.len() {
        1
    } else {
        0
    }
}

fn target_keys(app: &App) -> Vec<String> {
    if app.active_mask.is_some() {
        app.filtered_objects.iter().map(|o| o.key.clone()).collect()
    } else {
        app.objects
            .get(app.selected_object)
            .map(|o| vec![o.key.clone()])
            .unwrap_or_default()
    }
}

fn draw(frame: &mut ratatui::Frame, app: &App) {
    let size = frame.size();
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(10), Constraint::Length(6)])
        .split(size);

    let main = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(25),
            Constraint::Percentage(45),
            Constraint::Percentage(30),
        ])
        .split(vertical[0]);

    draw_buckets(frame, main[0], app);
    draw_objects(frame, main[1], app);
    draw_side_panel(frame, main[2], app);
    draw_status(frame, vertical[1], app);

    match app.mode {
        AppMode::EditingMask => draw_mask_popup(frame, app),
        AppMode::SelectingStorageClass => draw_storage_popup(frame, app),
        AppMode::Confirming => draw_confirm_popup(frame, app),
        AppMode::ShowingHelp => draw_help_popup(frame),
        AppMode::ViewingLog => draw_log_popup(frame, app),
        AppMode::Browsing => {}
    }
}

fn draw_buckets(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let title = format!("Buckets ({}) – Enter to load objects", app.buckets.len());
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(highlight_border(app.active_pane == ActivePane::Buckets));
    let items: Vec<ListItem> = app
        .buckets
        .iter()
        .enumerate()
        .map(|(idx, bucket)| {
            let subtitle = bucket
                .region
                .clone()
                .unwrap_or_else(|| "region unresolved".into());
            let is_selected = idx == app.selected_bucket;
            let marker = if is_selected { ">" } else { " " };
            let marker_style = if is_selected {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            ListItem::new(Line::from(vec![
                Span::styled(marker.to_string(), marker_style),
                Span::raw(" "),
                Span::styled(&bucket.name, Style::default().add_modifier(Modifier::BOLD)),
                Span::raw(" "),
                Span::styled(subtitle, Style::default().fg(Color::Gray)),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    state.select(Some(
        app.selected_bucket.min(app.buckets.len().saturating_sub(1)),
    ));
    let list = List::new(items)
        .highlight_style(Style::default().fg(Color::Cyan))
        .block(block);
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_objects(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let objects = app.active_objects();
    let title = if let Some(mask) = &app.active_mask {
        format!("Objects – mask: {}", mask.summary())
    } else {
        "Objects".to_string()
    };
    let block = Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(highlight_border(app.active_pane == ActivePane::Objects));
    let items: Vec<ListItem> = objects
        .iter()
        .enumerate()
        .map(|(idx, obj)| {
            let is_selected = idx == app.selected_object;
            let marker = if is_selected { ">" } else { " " };
            let marker_style = if is_selected {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            ListItem::new(Line::from(vec![
                Span::styled(marker.to_string(), marker_style),
                Span::raw(" "),
                Span::raw(obj.key.clone()),
                Span::raw(" "),
                Span::styled(format_size(obj.size), Style::default().fg(Color::Gray)),
                Span::raw(" "),
                Span::styled(
                    obj.storage_class.label(),
                    Style::default().fg(Color::Yellow),
                ),
            ]))
        })
        .collect();
    let mut state = ListState::default();
    if !objects.is_empty() {
        state.select(Some(app.selected_object.min(objects.len() - 1)));
    }
    let list = List::new(items)
        .highlight_style(Style::default().bg(Color::Blue).fg(Color::Black))
        .block(block);
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_side_panel(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(8),
            Constraint::Length(7),
            Constraint::Min(5),
        ])
        .split(area);

    draw_object_detail(frame, chunks[0], app);
    draw_mask_panel(frame, chunks[1], app);
    draw_policy_panel(frame, chunks[2], app);
}

fn draw_object_detail(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let block = Block::default()
        .title("Selected object")
        .borders(Borders::ALL);
    let lines = if let Some(obj) = app.selected_object() {
        let modified = obj
            .last_modified
            .clone()
            .unwrap_or_else(|| "unknown".into());
        let restore = obj
            .restore_state
            .as_ref()
            .map(describe_restore_state)
            .unwrap_or_else(|| "n/a".into());
        vec![
            Line::from(format!("Key: {}", obj.key)),
            Line::from(format!("Size: {}", format_size(obj.size))),
            Line::from(format!("Storage: {}", obj.storage_class.label())),
            Line::from(format!("Last modified: {}", modified)),
            Line::from(format!("Restore: {}", restore)),
        ]
    } else {
        vec![Line::from("No object selected")]
    };
    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    frame.render_widget(para, area);
}

fn draw_mask_panel(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let block = Block::default()
        .title("Mask")
        .borders(Borders::ALL)
        .border_style(highlight_border(app.active_pane == ActivePane::MaskEditor));
    let mut lines = Vec::new();
    if let Some(mask) = &app.active_mask {
        lines.push(Line::from(format!("Active: {}", mask.summary())));
        lines.push(Line::from(format!(
            "{} objects currently targeted",
            app.filtered_objects.len()
        )));
    } else {
        lines.push(Line::from("No active mask. Press 'm' to edit."));
    }
    lines.push(Line::from(
        "Mask tips: Tab cycles fields · arrows/space adjust options · Enter applies · Esc cancels",
    ));
    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    frame.render_widget(para, area);
}

fn draw_policy_panel(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let block = Block::default()
        .title("Policies")
        .borders(Borders::ALL)
        .border_style(highlight_border(app.active_pane == ActivePane::Policies));
    let lines: Vec<Line> = app
        .policies
        .iter()
        .take(4)
        .map(|policy| {
            Line::from(format!(
                "{} -> {} ({})",
                policy.mask.name,
                policy.target_storage_class.label(),
                policy.bucket
            ))
        })
        .collect();
    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    frame.render_widget(para, area);
}

fn draw_status(frame: &mut ratatui::Frame, area: Rect, app: &App) {
    let help = Line::from(vec![
        Span::raw("Tab switch "),
        Span::raw("m mask "),
        Span::raw("s storage "),
        Span::raw("p save-policy "),
        Span::raw("r restore "),
        Span::raw("i inspect "),
        Span::raw("f refresh "),
        Span::raw("Esc clear "),
        Span::raw("? help "),
        Span::raw("l log "),
        Span::raw("q quit"),
    ]);
    let mut lines = vec![help];
    lines.extend(app.status.iter().rev().map(|msg| Line::from(msg.clone())));
    let block = Block::default().borders(Borders::ALL).title("Status");
    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    frame.render_widget(para, area);
}

fn draw_mask_popup(frame: &mut ratatui::Frame, app: &App) {
    let area = centered_rect(60, 30, frame.size());
    draw_modal_surface(frame, area);
    let block = Block::default()
        .title("Mask editor – Tab moves fields, arrows/space adjust options, Enter applies, Esc cancels")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black));
    let text = vec![
        field_line(
            "Name: ",
            &app.mask_draft.name,
            matches!(app.mask_field, MaskEditorField::Name),
        ),
        field_line(
            "Pattern: ",
            &app.mask_draft.pattern,
            matches!(app.mask_field, MaskEditorField::Pattern),
        ),
        Line::from(vec![
            Span::styled(
                "Match mode: ",
                mask_field_style(matches!(app.mask_field, MaskEditorField::Mode)),
            ),
            Span::raw(app.mask_draft.kind.to_string()),
            Span::raw("  (use ←/→ or space)"),
        ]),
        Line::from(vec![
            Span::styled(
                "Case sensitive: ",
                mask_field_style(matches!(app.mask_field, MaskEditorField::Case)),
            ),
            Span::raw(if app.mask_draft.case_sensitive {
                "on"
            } else {
                "off"
            }),
            Span::raw("  (space or 'c' toggles)"),
        ]),
        Line::from("Enter applies the mask. Esc cancels and restores previous filter."),
    ];
    let para = Paragraph::new(text).block(block).wrap(Wrap { trim: true });
    frame.render_widget(para, area);
}

fn draw_storage_popup(frame: &mut ratatui::Frame, app: &App) {
    let area = centered_rect(40, 50, frame.size());
    draw_modal_surface(frame, area);
    let block = Block::default()
        .title("Select storage class (Enter confirm, Esc cancel)")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black));
    let items: Vec<ListItem> = StorageClassTier::selectable()
        .iter()
        .map(|class| ListItem::new(class.label()))
        .collect();
    let mut state = ListState::default();
    state.select(Some(app.storage_class_cursor));
    let list = List::new(items)
        .block(block)
        .highlight_style(Style::default().fg(Color::Black).bg(Color::Yellow));
    frame.render_stateful_widget(list, area, &mut state);
}

fn draw_confirm_popup(frame: &mut ratatui::Frame, app: &App) {
    let area = centered_rect(50, 30, frame.size());
    draw_modal_surface(frame, area);
    let mut lines = vec![Line::from(
        "Confirm operation (Enter/y to proceed, Esc/n to cancel, o toggle restore-first)",
    )];
    if let Some(action) = &app.pending_action {
        match action {
            PendingAction::Transition {
                target_class,
                restore_first,
            } => {
                lines.push(Line::from(format!(
                    "Transition {} object(s) to {}",
                    target_count(app),
                    target_class.label()
                )));
                lines.push(Line::from(format!(
                    "Restore before transition: {}",
                    if *restore_first { "yes" } else { "no" }
                )));
            }
            PendingAction::Restore { days } => {
                lines.push(Line::from(format!(
                    "Request restore for {} object(s) ({} days)",
                    target_count(app),
                    days
                )));
            }
            PendingAction::SavePolicy { target_class } => {
                lines.push(Line::from("Save policy with current mask"));
                lines.push(Line::from(format!(
                    "Bucket: {}",
                    app.selected_bucket_name().unwrap_or("n/a")
                )));
                lines.push(Line::from(format!(
                    "Target storage class: {}",
                    target_class.label()
                )));
            }
        }
    }
    let block = Block::default()
        .title("Confirm")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black));
    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    frame.render_widget(para, area);
}

fn draw_help_popup(frame: &mut ratatui::Frame) {
    let area = centered_rect(70, 70, frame.size());
    draw_modal_surface(frame, area);
    let block = Block::default()
        .title("Cheat Sheet – Esc/?/Enter to close")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black));
    let lines = vec![
        Line::from(
            "Navigation: Tab/Shift+Tab switch panes · Arrows/pg keys move · Enter loads bucket objects",
        ),
        Line::from(
            "Masks: press 'm' to edit · Tab moves between fields · arrows/space adjust match mode/case · Enter applies",
        ),
        Line::from(
            "Apply mask with Enter, Esc cancels; active mask targets operations at all matches.",
        ),
        Line::from(
            "Storage: 's' selects destination class, 'o' toggles restore-first during confirmation, Enter accepts.",
        ),
        Line::from("Policies: 'p' uses current mask + bucket, then choose a target class to save."),
        Line::from(
            "Restores: 'r' requests 7-day restore for selected/masked objects; 'i' refreshes metadata via HeadObject.",
        ),
        Line::from(
            "Logs: 'l' opens the status log overlay for full error messages; 'f' refreshes buckets; Esc clears masks/dialogs; 'q'/Ctrl+C quits.",
        ),
    ];
    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    frame.render_widget(para, area);
}

fn draw_log_popup(frame: &mut ratatui::Frame, app: &App) {
    let area = centered_rect(70, 60, frame.size());
    draw_modal_surface(frame, area);
    let block = Block::default()
        .title("Status log – Esc/l/Enter to close")
        .borders(Borders::ALL)
        .style(Style::default().bg(Color::Black));
    let mut lines: Vec<Line> = app
        .status
        .iter()
        .rev()
        .enumerate()
        .map(|(idx, msg)| Line::from(format!("{:>2}. {}", idx + 1, msg)))
        .collect();
    if lines.is_empty() {
        lines.push(Line::from("No status messages yet."));
    }
    let para = Paragraph::new(lines).block(block).wrap(Wrap { trim: true });
    frame.render_widget(para, area);
}

fn draw_modal_surface(frame: &mut ratatui::Frame, area: Rect) {
    frame.render_widget(Clear, area);
    let backdrop = Block::default().style(Style::default().bg(Color::Black));
    frame.render_widget(backdrop, area);

    let canvas = frame.size();
    let shadow_style = Style::default().bg(Color::DarkGray);
    if area.y + area.height < canvas.height {
        let shadow_width = area.width.min(canvas.width.saturating_sub(area.x + 1));
        if shadow_width > 0 {
            let shadow = Rect::new(area.x + 1, area.y + area.height, shadow_width, 1);
            frame.render_widget(Block::default().style(shadow_style), shadow);
        }
    }
    if area.x + area.width < canvas.width {
        let shadow_height = area.height.min(canvas.height.saturating_sub(area.y + 1));
        if shadow_height > 0 {
            let shadow = Rect::new(area.x + area.width, area.y + 1, 1, shadow_height);
            frame.render_widget(Block::default().style(shadow_style), shadow);
        }
    }
}

fn field_line(label: &str, value: &str, selected: bool) -> Line<'static> {
    Line::from(vec![
        Span::styled(label.to_string(), mask_field_style(selected)),
        Span::raw(value.to_string()),
    ])
}

fn mask_field_style(selected: bool) -> Style {
    if selected {
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    }
}

fn describe_restore_state(state: &RestoreState) -> String {
    match state {
        RestoreState::Available => "available".into(),
        RestoreState::Expired => "expired".into(),
        RestoreState::InProgress { expiry } => {
            if let Some(expiry) = expiry {
                format!("in-progress (ready until {expiry})")
            } else {
                "in-progress".into()
            }
        }
    }
}

fn describe_restore_error(err: &anyhow::Error) -> String {
    if let Some(sdk_err) = err.downcast_ref::<SdkError<RestoreObjectError>>() {
        match sdk_err {
            SdkError::ServiceError(err) => {
                let service = err.err();
                let code = service.meta().code().unwrap_or("ServiceError");
                let message = service
                    .message()
                    .map(|m| m.to_string())
                    .unwrap_or_else(|| "no message provided".into());
                let friendly = match code {
                    "NoSuchKey" => {
                        "object was not found (mask may target stale keys or bucket differs)".into()
                    }
                    "InvalidObjectState" => {
                        "object is already being restored or not eligible for this operation".into()
                    }
                    _ => message.clone(),
                };
                if matches!(code, "NoSuchKey" | "InvalidObjectState") {
                    return format!("{code}: {friendly}");
                }
                return format!("{code}: {message}");
            }
            SdkError::DispatchFailure(err) => {
                return format!("network/dispatch failure: {err:?}");
            }
            SdkError::TimeoutError(_) => {
                return "request timed out; please retry".into();
            }
            SdkError::ResponseError(ctx) => {
                return format!("response error: {ctx:?}");
            }
            _ => {}
        }
    }
    format!("{err:#}")
}

fn centered_rect(width_percent: u16, height_percent: u16, area: Rect) -> Rect {
    let vertical = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - height_percent) / 2),
            Constraint::Percentage(height_percent),
            Constraint::Percentage((100 - height_percent) / 2),
        ])
        .split(area);
    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - width_percent) / 2),
            Constraint::Percentage(width_percent),
            Constraint::Percentage((100 - width_percent) / 2),
        ])
        .split(vertical[1])[1]
}

fn highlight_border(active: bool) -> Style {
    if active {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default()
    }
}

fn format_size(size: i64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    if size as f64 > GB {
        format!("{:.2} GB", size as f64 / GB)
    } else if size as f64 > MB {
        format!("{:.2} MB", size as f64 / MB)
    } else if size as f64 > KB {
        format!("{:.2} KB", size as f64 / KB)
    } else {
        format!("{size} B")
    }
}
