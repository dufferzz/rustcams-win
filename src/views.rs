use crate::layout::Layout;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedView {
    pub name: String,
    pub layout: String,
    /// Camera ids; empty string or missing = empty slot.
    pub slots: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ViewsFile {
    #[serde(default)]
    pub active: usize,
    #[serde(default)]
    pub views: Vec<SavedView>,
}

#[derive(Debug, Clone)]
pub struct View {
    pub name: String,
    pub layout: Layout,
    pub slots: Vec<Option<String>>,
}

impl View {
    pub fn new(name: impl Into<String>, layout: Layout) -> Self {
        let n = layout.cells();
        Self {
            name: name.into(),
            layout,
            slots: vec![None; n],
        }
    }

    pub fn resize_for_layout(&mut self, layout: Layout) {
        let n = layout.cells();
        self.layout = layout;
        if self.slots.len() < n {
            self.slots.resize(n, None);
        } else if self.slots.len() > n {
            self.slots.truncate(n);
        }
    }

    pub fn from_saved(saved: &SavedView) -> Self {
        let layout = Layout::from_str(&saved.layout);
        let mut slots: Vec<Option<String>> = saved
            .slots
            .iter()
            .map(|id| {
                if id.is_empty() {
                    None
                } else {
                    Some(id.clone())
                }
            })
            .collect();
        let n = layout.cells();
        if slots.len() < n {
            slots.resize(n, None);
        } else if slots.len() > n {
            slots.truncate(n);
        }
        Self {
            name: saved.name.clone(),
            layout,
            slots,
        }
    }

    pub fn to_saved(&self) -> SavedView {
        SavedView {
            name: self.name.clone(),
            layout: self.layout.as_str().into(),
            slots: self
                .slots
                .iter()
                .map(|s| s.clone().unwrap_or_default())
                .collect(),
        }
    }

    pub fn fill_from_cameras(&mut self, camera_ids: &[String]) {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            *slot = camera_ids.get(i).cloned();
        }
    }
}

pub struct ViewStore {
    pub path: PathBuf,
    pub views: Vec<View>,
    pub active: usize,
    dirty: bool,
}

impl ViewStore {
    pub fn load_or_default(
        path: impl AsRef<Path>,
        camera_ids: &[String],
        default_layout: Layout,
    ) -> Self {
        let path = path.as_ref().to_path_buf();
        if let Ok(text) = fs::read_to_string(&path) {
            if let Ok(file) = toml::from_str::<ViewsFile>(&text) {
                if !file.views.is_empty() {
                    let views: Vec<View> = file.views.iter().map(View::from_saved).collect();
                    let active = file.active.min(views.len().saturating_sub(1));
                    return Self {
                        path,
                        views,
                        active,
                        dirty: false,
                    };
                }
            }
        }

        let mut main = View::new("Main", default_layout);
        main.fill_from_cameras(camera_ids);
        Self {
            path,
            views: vec![main],
            active: 0,
            dirty: true,
        }
    }

    pub fn clamp_index(&self, idx: usize) -> usize {
        if self.views.is_empty() {
            0
        } else {
            idx.min(self.views.len() - 1)
        }
    }

    pub fn view(&self, idx: usize) -> &View {
        &self.views[self.clamp_index(idx)]
    }

    pub fn view_mut(&mut self, idx: usize) -> &mut View {
        let i = self.clamp_index(idx);
        &mut self.views[i]
    }

    pub fn active_view(&self) -> &View {
        self.view(self.active)
    }

    pub fn active_view_mut(&mut self) -> &mut View {
        let i = self.active;
        self.view_mut(i)
    }

    pub fn mark_dirty(&mut self) {
        self.dirty = true;
    }

    pub fn save_if_dirty(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        self.save()
    }

    pub fn save(&mut self) -> Result<()> {
        let file = ViewsFile {
            active: self.active,
            views: self.views.iter().map(View::to_saved).collect(),
        };
        let text = toml::to_string_pretty(&file).context("serialize views")?;
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        fs::write(&self.path, text).with_context(|| format!("write {}", self.path.display()))?;
        self.dirty = false;
        Ok(())
    }
}
