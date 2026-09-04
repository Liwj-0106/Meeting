use serde::{Deserialize, Serialize};
use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;
use tauri::{AppHandle, Emitter, LogicalSize, Manager, PhysicalPosition, PhysicalSize, Runtime};

pub const CAPTION_OVERLAY_LABEL: &str = "caption-overlay";
pub const CAPTION_SETTINGS_LABEL: &str = "caption-settings";

const SETTINGS_FILE_NAME: &str = "caption-overlay-settings.json";
const SETTINGS_CHANGED_EVENT: &str = "caption-overlay-settings-changed";

const MIN_WINDOW_WIDTH: u32 = 360;
const MAX_WINDOW_WIDTH: u32 = 1600;
const MIN_WINDOW_HEIGHT: u32 = 100;
const MAX_WINDOW_HEIGHT: u32 = 480;
const MIN_FONT_SIZE: u16 = 16;
const MAX_FONT_SIZE: u16 = 56;
const SETTINGS_LOGICAL_WIDTH: f64 = 460.0;
const SETTINGS_LOGICAL_HEIGHT: f64 = 440.0;
const SETTINGS_WORK_AREA_MARGIN: u32 = 8;
const SETTINGS_WINDOW_GAP: i32 = 10;

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptionOverlayMode {
    #[default]
    Captions,
    Highlights,
    Actions,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(default)]
pub struct CaptionOverlaySettings {
    pub window_width: u32,
    pub window_height: u32,
    pub font_size: u16,
    pub background_opacity: u8,
    pub mouse_passthrough: bool,
    pub content_protection: bool,
    pub assistant_mode: CaptionOverlayMode,
    pub window_x: Option<i32>,
    pub window_y: Option<i32>,
}

impl Default for CaptionOverlaySettings {
    fn default() -> Self {
        Self {
            window_width: 900,
            window_height: 180,
            font_size: 30,
            background_opacity: 80,
            mouse_passthrough: false,
            content_protection: false,
            assistant_mode: CaptionOverlayMode::Captions,
            window_x: None,
            window_y: None,
        }
    }
}

impl CaptionOverlaySettings {
    fn normalized(mut self) -> Self {
        self.window_width = self.window_width.clamp(MIN_WINDOW_WIDTH, MAX_WINDOW_WIDTH);
        self.window_height = self
            .window_height
            .clamp(MIN_WINDOW_HEIGHT, MAX_WINDOW_HEIGHT);
        self.font_size = self.font_size.clamp(MIN_FONT_SIZE, MAX_FONT_SIZE);
        self.background_opacity = self.background_opacity.min(100);
        self
    }
}

static SETTINGS: LazyLock<Mutex<CaptionOverlaySettings>> =
    LazyLock::new(|| Mutex::new(CaptionOverlaySettings::default()));
static MOVE_SAVE_REVISION: AtomicU64 = AtomicU64::new(0);

fn settings_snapshot() -> CaptionOverlaySettings {
    SETTINGS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn replace_settings(settings: CaptionOverlaySettings) {
    *SETTINGS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = settings;
}

fn load_settings<R: Runtime>(app: &AppHandle<R>) -> Result<CaptionOverlaySettings, String> {
    let path = crate::storage::store_path(app, SETTINGS_FILE_NAME)
        .map_err(|error| format!("Failed to resolve caption settings path: {error}"))?;

    match fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str::<CaptionOverlaySettings>(&content) {
            Ok(settings) => Ok(settings.normalized()),
            Err(error) => {
                log::warn!(
                    "Ignoring invalid caption overlay settings at {}: {}",
                    path.display(),
                    error
                );
                Ok(CaptionOverlaySettings::default())
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(CaptionOverlaySettings::default())
        }
        Err(error) => Err(format!(
            "Failed to read caption overlay settings {}: {error}",
            path.display()
        )),
    }
}

fn save_settings<R: Runtime>(
    app: &AppHandle<R>,
    settings: &CaptionOverlaySettings,
) -> Result<(), String> {
    let path = crate::storage::store_path(app, SETTINGS_FILE_NAME)
        .map_err(|error| format!("Failed to resolve caption settings path: {error}"))?;

    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Failed to create caption settings directory {}: {error}",
                parent.display()
            )
        })?;
    }

    let content = serde_json::to_string_pretty(settings)
        .map_err(|error| format!("Failed to serialize caption settings: {error}"))?;
    fs::write(&path, content).map_err(|error| {
        format!(
            "Failed to save caption overlay settings {}: {error}",
            path.display()
        )
    })
}

fn overlay_window<R: Runtime>(app: &AppHandle<R>) -> Result<tauri::WebviewWindow<R>, String> {
    app.get_webview_window(CAPTION_OVERLAY_LABEL)
        .ok_or_else(|| "Caption overlay window is not available".to_string())
}

fn settings_window<R: Runtime>(app: &AppHandle<R>) -> Result<tauri::WebviewWindow<R>, String> {
    app.get_webview_window(CAPTION_SETTINGS_LABEL)
        .ok_or_else(|| "Caption settings window is not available".to_string())
}

fn clamp_to_work_area(
    position: PhysicalPosition<i32>,
    window_size: PhysicalSize<u32>,
    work_position: PhysicalPosition<i32>,
    work_size: PhysicalSize<u32>,
) -> PhysicalPosition<i32> {
    let left = work_position.x;
    let top = work_position.y;
    let right = left.saturating_add(work_size.width as i32);
    let bottom = top.saturating_add(work_size.height as i32);
    let max_x = right.saturating_sub(window_size.width as i32).max(left);
    let max_y = bottom.saturating_sub(window_size.height as i32).max(top);
    PhysicalPosition::new(position.x.clamp(left, max_x), position.y.clamp(top, max_y))
}

fn squared_distance_to_work_area(
    position: PhysicalPosition<i32>,
    work_position: PhysicalPosition<i32>,
    work_size: PhysicalSize<u32>,
) -> i64 {
    let right = work_position
        .x
        .saturating_add(work_size.width as i32)
        .saturating_sub(1);
    let bottom = work_position
        .y
        .saturating_add(work_size.height as i32)
        .saturating_sub(1);
    let nearest_x = position
        .x
        .clamp(work_position.x, right.max(work_position.x));
    let nearest_y = position
        .y
        .clamp(work_position.y, bottom.max(work_position.y));
    let dx = i64::from(position.x) - i64::from(nearest_x);
    let dy = i64::from(position.y) - i64::from(nearest_y);
    dx.saturating_mul(dx).saturating_add(dy.saturating_mul(dy))
}

fn clamp_overlay_to_visible_work_area<R: Runtime>(
    window: &tauri::WebviewWindow<R>,
    desired: PhysicalPosition<i32>,
) -> Result<PhysicalPosition<i32>, String> {
    let monitors = window
        .available_monitors()
        .map_err(|error| format!("Failed to enumerate caption monitors: {error}"))?;
    let Some(monitor) = monitors.iter().min_by_key(|monitor| {
        let work_area = monitor.work_area();
        squared_distance_to_work_area(desired, work_area.position, work_area.size)
    }) else {
        return Ok(desired);
    };
    let work_area = monitor.work_area();
    let window_size = window
        .outer_size()
        .map_err(|error| format!("Failed to read caption overlay size: {error}"))?;
    Ok(clamp_to_work_area(
        desired,
        window_size,
        work_area.position,
        work_area.size,
    ))
}

fn restore_overlay_position<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let settings = settings_snapshot();
    let (Some(window_x), Some(window_y)) = (settings.window_x, settings.window_y) else {
        return Ok(());
    };
    let window = overlay_window(app)?;
    let position =
        clamp_overlay_to_visible_work_area(&window, PhysicalPosition::new(window_x, window_y))?;
    window
        .set_position(position)
        .map_err(|error| format!("Failed to restore caption overlay position: {error}"))
}

fn fitted_settings_size(scale_factor: f64, work_size: PhysicalSize<u32>) -> PhysicalSize<u32> {
    let safe_scale_factor = if scale_factor.is_finite() && scale_factor > 0.0 {
        scale_factor
    } else {
        1.0
    };
    let desired: PhysicalSize<u32> =
        LogicalSize::new(SETTINGS_LOGICAL_WIDTH, SETTINGS_LOGICAL_HEIGHT)
            .to_physical(safe_scale_factor);
    let reserved_margin = SETTINGS_WORK_AREA_MARGIN.saturating_mul(2);
    let available_width = work_size.width.saturating_sub(reserved_margin).max(1);
    let available_height = work_size.height.saturating_sub(reserved_margin).max(1);
    PhysicalSize::new(
        desired.width.min(available_width),
        desired.height.min(available_height),
    )
}

fn settings_position_in_work_area(
    overlay_position: PhysicalPosition<i32>,
    overlay_size: PhysicalSize<u32>,
    settings_size: PhysicalSize<u32>,
    work_position: PhysicalPosition<i32>,
    work_size: PhysicalSize<u32>,
) -> PhysicalPosition<i32> {
    let horizontal_spare = work_size.width.saturating_sub(settings_size.width) as i32;
    let vertical_spare = work_size.height.saturating_sub(settings_size.height) as i32;
    let horizontal_margin = (SETTINGS_WORK_AREA_MARGIN as i32).min(horizontal_spare / 2);
    let vertical_margin = (SETTINGS_WORK_AREA_MARGIN as i32).min(vertical_spare / 2);

    let work_left = work_position.x.saturating_add(horizontal_margin);
    let work_top = work_position.y.saturating_add(vertical_margin);
    let work_right = work_position
        .x
        .saturating_add(work_size.width as i32)
        .saturating_sub(horizontal_margin);
    let work_bottom = work_position
        .y
        .saturating_add(work_size.height as i32)
        .saturating_sub(vertical_margin);
    let settings_width = settings_size.width as i32;
    let settings_height = settings_size.height as i32;
    let max_x = work_right.saturating_sub(settings_width).max(work_left);
    let max_y = work_bottom.saturating_sub(settings_height).max(work_top);

    let aligned_x = overlay_position
        .x
        .saturating_add(overlay_size.width as i32)
        .saturating_sub(settings_width);
    let x = aligned_x.clamp(work_left, max_x);

    let below_y = overlay_position
        .y
        .saturating_add(overlay_size.height as i32)
        .saturating_add(SETTINGS_WINDOW_GAP);
    let above_y = overlay_position
        .y
        .saturating_sub(settings_height)
        .saturating_sub(SETTINGS_WINDOW_GAP);
    let y = if below_y.saturating_add(settings_height) <= work_bottom {
        below_y
    } else if above_y >= work_top {
        above_y
    } else {
        overlay_position.y.clamp(work_top, max_y)
    };

    PhysicalPosition::new(x, y)
}

fn position_settings_window<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let overlay = overlay_window(app)?;
    let settings = settings_window(app)?;
    let overlay_position = overlay
        .outer_position()
        .map_err(|error| format!("Failed to read caption overlay position: {error}"))?;
    let overlay_size = overlay
        .outer_size()
        .map_err(|error| format!("Failed to read caption overlay size: {error}"))?;
    let Some(monitor) = overlay
        .current_monitor()
        .map_err(|error| format!("Failed to read the caption overlay monitor: {error}"))?
        .or_else(|| overlay.primary_monitor().ok().flatten())
    else {
        return settings
            .center()
            .map_err(|error| format!("Failed to center caption settings: {error}"));
    };

    let work_area = monitor.work_area();
    let target_size = fitted_settings_size(monitor.scale_factor(), work_area.size);
    settings
        .set_size(target_size)
        .map_err(|error| format!("Failed to fit caption settings to the monitor: {error}"))?;
    let settings_size = settings.outer_size().unwrap_or(target_size);
    let position = settings_position_in_work_area(
        overlay_position,
        overlay_size,
        settings_size,
        work_area.position,
        work_area.size,
    );

    settings
        .set_position(position)
        .map_err(|error| format!("Failed to position caption settings: {error}"))
}

fn apply_settings_visibility<R: Runtime>(
    app: &AppHandle<R>,
    visible: bool,
) -> Result<bool, String> {
    let window = settings_window(app)?;

    if visible {
        if let Err(error) = position_settings_window(app) {
            log::warn!("Failed to pre-position caption settings: {}", error);
        }
        window.show().map_err(|error| error.to_string())?;
        if let Err(error) = position_settings_window(app) {
            log::warn!("Failed to position visible caption settings: {}", error);
        }
        if let Err(error) = window.set_focus() {
            log::warn!("Failed to focus caption settings: {}", error);
        }
    } else {
        window.hide().map_err(|error| error.to_string())?;
    }

    Ok(visible)
}

fn apply_runtime_settings<R: Runtime>(
    app: &AppHandle<R>,
    settings: &CaptionOverlaySettings,
) -> Result<(), String> {
    let window = overlay_window(app)?;
    window
        .set_size(LogicalSize::new(
            settings.window_width as f64,
            settings.window_height as f64,
        ))
        .map_err(|error| format!("Failed to resize caption overlay: {error}"))?;
    window
        .set_content_protected(settings.content_protection)
        .map_err(|error| format!("Failed to change caption content protection: {error}"))?;

    if let Ok(current_position) = window.outer_position() {
        let visible_position = clamp_overlay_to_visible_work_area(&window, current_position)?;
        if visible_position != current_position {
            window
                .set_position(visible_position)
                .map_err(|error| format!("Failed to keep caption overlay on screen: {error}"))?;
        }
    }

    // On Linux the native click-through implementation requires a realized
    // window. Keep the desired state in SETTINGS while hidden and apply it
    // immediately after the overlay is shown.
    if window.is_visible().map_err(|error| error.to_string())? {
        window
            .set_ignore_cursor_events(settings.mouse_passthrough)
            .map_err(|error| format!("Failed to change caption mouse passthrough: {error}"))?;
    }

    Ok(())
}

fn notify_settings<R: Runtime>(app: &AppHandle<R>, settings: &CaptionOverlaySettings) {
    if let Err(error) = app.emit(SETTINGS_CHANGED_EVENT, settings.clone()) {
        log::warn!("Failed to emit caption overlay settings: {}", error);
    }
}

pub fn initialize<R: Runtime>(app: &AppHandle<R>) -> Result<(), String> {
    let settings = load_settings(app)?;
    replace_settings(settings.clone());

    // The overlay starts hidden. Applying the size is safe; click-through is
    // deferred until apply_visibility shows the native window.
    apply_runtime_settings(app, &settings)?;
    restore_overlay_position(app)
}

pub fn overlay_is_visible<R: Runtime>(app: &AppHandle<R>) -> Result<bool, String> {
    overlay_window(app)?
        .is_visible()
        .map_err(|error| error.to_string())
}

pub fn mouse_passthrough_enabled() -> bool {
    settings_snapshot().mouse_passthrough
}

pub fn notify_visibility<R: Runtime>(app: &AppHandle<R>, visible: bool) {
    if let Err(error) = app.emit(
        "caption-overlay-visibility-changed",
        serde_json::json!({ "visible": visible }),
    ) {
        log::warn!("Failed to emit caption overlay visibility: {}", error);
    }
}

pub fn handle_overlay_moved<R: Runtime>(app: AppHandle<R>, position: PhysicalPosition<i32>) {
    let mut settings = settings_snapshot();
    if settings.window_x == Some(position.x) && settings.window_y == Some(position.y) {
        return;
    }
    settings.window_x = Some(position.x);
    settings.window_y = Some(position.y);
    replace_settings(settings);

    if settings_window(&app)
        .and_then(|window| window.is_visible().map_err(|error| error.to_string()))
        .unwrap_or(false)
    {
        if let Err(error) = position_settings_window(&app) {
            log::warn!("Failed to follow the moved caption overlay: {}", error);
        }
    }

    let revision = MOVE_SAVE_REVISION.fetch_add(1, Ordering::SeqCst) + 1;
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        if MOVE_SAVE_REVISION.load(Ordering::SeqCst) != revision {
            return;
        }
        let settings = settings_snapshot();
        if let Err(error) = save_settings(&app, &settings) {
            log::warn!("Failed to persist caption overlay position: {}", error);
        } else {
            notify_settings(&app, &settings);
        }
    });
}

fn apply_visibility<R: Runtime>(app: &AppHandle<R>, visible: bool) -> Result<bool, String> {
    let window = overlay_window(app)?;

    if visible {
        window.show().map_err(|error| error.to_string())?;
        if let Err(error) = apply_runtime_settings(app, &settings_snapshot()) {
            let _ = window.hide();
            return Err(error);
        }
    } else {
        if let Err(error) = apply_settings_visibility(app, false) {
            log::warn!(
                "Failed to hide caption settings with the overlay: {}",
                error
            );
        }
        window.hide().map_err(|error| error.to_string())?;
    }

    notify_visibility(app, visible);
    crate::tray::update_tray_menu(app);
    Ok(visible)
}

fn update_settings<R: Runtime>(
    app: &AppHandle<R>,
    settings: CaptionOverlaySettings,
) -> Result<CaptionOverlaySettings, String> {
    let next_settings = settings.normalized();
    let previous_settings = settings_snapshot();

    if let Err(error) = apply_runtime_settings(app, &next_settings) {
        let _ = apply_runtime_settings(app, &previous_settings);
        return Err(error);
    }

    if let Err(error) = save_settings(app, &next_settings) {
        let _ = apply_runtime_settings(app, &previous_settings);
        return Err(error);
    }

    replace_settings(next_settings.clone());
    notify_settings(app, &next_settings);
    if settings_window(app)
        .and_then(|window| window.is_visible().map_err(|error| error.to_string()))
        .unwrap_or(false)
    {
        if let Err(error) = position_settings_window(app) {
            log::warn!("Failed to follow the resized caption overlay: {}", error);
        }
    }
    crate::tray::update_tray_menu(app);
    Ok(next_settings)
}

#[tauri::command]
pub fn set_caption_overlay_visible<R: Runtime>(
    app: AppHandle<R>,
    visible: bool,
) -> Result<bool, String> {
    apply_visibility(&app, visible)
}

#[tauri::command]
pub fn is_caption_overlay_visible<R: Runtime>(app: AppHandle<R>) -> Result<bool, String> {
    overlay_is_visible(&app)
}

#[tauri::command]
pub fn set_caption_settings_visible<R: Runtime>(
    app: AppHandle<R>,
    visible: bool,
) -> Result<bool, String> {
    apply_settings_visibility(&app, visible)
}

#[tauri::command]
pub fn get_caption_overlay_settings() -> CaptionOverlaySettings {
    settings_snapshot()
}

#[tauri::command]
pub fn set_caption_overlay_settings<R: Runtime>(
    app: AppHandle<R>,
    settings: CaptionOverlaySettings,
) -> Result<CaptionOverlaySettings, String> {
    update_settings(&app, settings)
}

pub fn toggle_caption_overlay<R: Runtime>(app: &AppHandle<R>) -> Result<bool, String> {
    let should_show = !overlay_is_visible(app)?;
    apply_visibility(app, should_show)
}

pub fn toggle_mouse_passthrough<R: Runtime>(app: &AppHandle<R>) -> Result<bool, String> {
    let mut settings = settings_snapshot();
    settings.mouse_passthrough = !settings.mouse_passthrough;
    Ok(update_settings(app, settings)?.mouse_passthrough)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_caption_overlay_settings() {
        let settings = CaptionOverlaySettings {
            window_width: 1,
            window_height: u32::MAX,
            font_size: u16::MAX,
            background_opacity: 255,
            mouse_passthrough: true,
            content_protection: true,
            assistant_mode: CaptionOverlayMode::Actions,
            window_x: Some(i32::MIN),
            window_y: Some(i32::MAX),
        }
        .normalized();

        assert_eq!(settings.window_width, MIN_WINDOW_WIDTH);
        assert_eq!(settings.window_height, MAX_WINDOW_HEIGHT);
        assert_eq!(settings.font_size, MAX_FONT_SIZE);
        assert_eq!(settings.background_opacity, 100);
        assert!(settings.mouse_passthrough);
        assert!(settings.content_protection);
        assert_eq!(settings.assistant_mode, CaptionOverlayMode::Actions);
        assert_eq!(settings.window_x, Some(i32::MIN));
        assert_eq!(settings.window_y, Some(i32::MAX));
    }

    #[test]
    fn missing_json_fields_use_defaults() {
        let settings: CaptionOverlaySettings = serde_json::from_str("{}").unwrap();
        assert_eq!(settings, CaptionOverlaySettings::default());
    }

    #[test]
    fn serializes_assistant_mode_as_stable_snake_case_value() {
        let mut settings = CaptionOverlaySettings::default();
        settings.assistant_mode = CaptionOverlayMode::Highlights;

        let json = serde_json::to_value(settings).unwrap();
        assert_eq!(json["assistant_mode"], "highlights");
    }

    #[test]
    fn clamps_saved_position_inside_negative_coordinate_work_area() {
        let work_position = PhysicalPosition::new(-1920, -200);
        let work_size = PhysicalSize::new(1920, 1080);
        let window_size = PhysicalSize::new(900, 180);

        assert_eq!(
            clamp_to_work_area(
                PhysicalPosition::new(-5000, 5000),
                window_size,
                work_position,
                work_size,
            ),
            PhysicalPosition::new(-1920, 700),
        );
    }

    #[test]
    fn fits_settings_inside_small_high_dpi_work_area() {
        assert_eq!(
            fitted_settings_size(2.0, PhysicalSize::new(800, 600)),
            PhysicalSize::new(784, 584),
        );
        assert_eq!(
            fitted_settings_size(1.25, PhysicalSize::new(1920, 1080)),
            PhysicalSize::new(575, 550),
        );
        assert_eq!(
            fitted_settings_size(1.5, PhysicalSize::new(1600, 900)),
            PhysicalSize::new(690, 660),
        );
    }

    #[test]
    fn places_settings_below_overlay_when_work_area_has_room() {
        assert_eq!(
            settings_position_in_work_area(
                PhysicalPosition::new(-1910, 20),
                PhysicalSize::new(500, 100),
                PhysicalSize::new(460, 440),
                PhysicalPosition::new(-1920, 0),
                PhysicalSize::new(1920, 1080),
            ),
            PhysicalPosition::new(-1870, 130),
        );
    }

    #[test]
    fn places_settings_above_overlay_near_bottom_edge() {
        assert_eq!(
            settings_position_in_work_area(
                PhysicalPosition::new(-1800, 850),
                PhysicalSize::new(900, 180),
                PhysicalSize::new(460, 440),
                PhysicalPosition::new(-1920, 0),
                PhysicalSize::new(1920, 1080),
            ),
            PhysicalPosition::new(-1360, 400),
        );
    }

    #[test]
    fn keeps_fitted_settings_inside_work_area_margin() {
        assert_eq!(
            settings_position_in_work_area(
                PhysicalPosition::new(400, 300),
                PhysicalSize::new(900, 180),
                PhysicalSize::new(784, 584),
                PhysicalPosition::new(100, 50),
                PhysicalSize::new(800, 600),
            ),
            PhysicalPosition::new(108, 58),
        );
    }
}
