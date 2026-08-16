use std::path::{Path, PathBuf};

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tao::window::Window;

const WINDOW_MANAGER_FILE_NAME: &str = "window-manager.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowManagerState {
    pub main: PersistedWindowState,
    pub status: PersistedWindowState,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedWindowState {
    pub width: f64,
    pub height: f64,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub visible: bool,
    pub open: bool,
    pub maximized: bool,
}

impl Default for WindowManagerState {
    fn default() -> Self {
        Self {
            main: PersistedWindowState {
                width: 1440.0,
                height: 960.0,
                x: None,
                y: None,
                visible: true,
                open: true,
                maximized: false,
            },
            status: PersistedWindowState {
                width: 920.0,
                height: 700.0,
                x: None,
                y: None,
                visible: true,
                open: false,
                maximized: false,
            },
        }
    }
}

impl Default for PersistedWindowState {
    fn default() -> Self {
        Self {
            width: 0.0,
            height: 0.0,
            x: None,
            y: None,
            visible: false,
            open: false,
            maximized: false,
        }
    }
}

impl WindowManagerState {
    pub fn load(user_dir: &Path) -> anyhow::Result<Self> {
        let path = Self::path(user_dir);
        if !path.exists() {
            return Ok(Self::default());
        }

        let raw = std::fs::read_to_string(&path)
            .with_context(|| format!("读取窗口状态文件失败: {}", path.display()))?;
        let mut state = serde_json::from_str::<Self>(&raw)
            .with_context(|| format!("解析窗口状态文件失败: {}", path.display()))?;
        state.normalize();
        Ok(state)
    }

    pub fn save(&self, user_dir: &Path) -> anyhow::Result<()> {
        let path = Self::path(user_dir);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("创建窗口状态目录失败: {}", parent.display()))?;
        }

        let tmp_path = path.with_extension("json.tmp");
        let data = serde_json::to_string_pretty(self).context("序列化窗口状态失败")?;
        std::fs::write(&tmp_path, data)
            .with_context(|| format!("写入窗口状态临时文件失败: {}", tmp_path.display()))?;
        std::fs::rename(&tmp_path, &path)
            .with_context(|| format!("保存窗口状态失败: {}", path.display()))?;
        Ok(())
    }

    pub fn capture_main(&mut self, window: &Window, visible: bool) {
        capture_window(window, &mut self.main, visible, true);
    }

    pub fn capture_status(&mut self, window: Option<&Window>) {
        match window {
            Some(window) => capture_window(window, &mut self.status, true, true),
            None => {
                self.status.open = false;
            }
        }
    }

    pub fn path(user_dir: &Path) -> PathBuf {
        user_dir.join(WINDOW_MANAGER_FILE_NAME)
    }

    fn normalize(&mut self) {
        self.main
            .normalize_against(&WindowManagerState::default().main);
        self.status
            .normalize_against(&WindowManagerState::default().status);
    }
}

fn capture_window(window: &Window, state: &mut PersistedWindowState, visible: bool, open: bool) {
    let size = window.inner_size();
    state.width = size.width as f64;
    state.height = size.height as f64;
    state.visible = visible;
    state.open = open;
    state.maximized = window.is_maximized();

    if let Ok(position) = window.outer_position() {
        state.x = Some(position.x as f64);
        state.y = Some(position.y as f64);
    }
}

impl PersistedWindowState {
    fn normalize_against(&mut self, fallback: &PersistedWindowState) {
        if self.width <= 0.0 {
            self.width = fallback.width;
        }
        if self.height <= 0.0 {
            self.height = fallback.height;
        }

        if !self.open {
            self.visible = false;
        }
    }
}
