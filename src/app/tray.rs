//! Notification-area icon with quick profile switching, boost and pause.

use super::icon_draw::logo_rgba;
use hushcore::engine::Snapshot;
use tray_icon::menu::{CheckMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{TrayIcon, TrayIconBuilder};

pub struct Tray {
    icon: TrayIcon,
    pub open: MenuItem,
    pub auto: CheckMenuItem,
    pub profiles: Vec<(String, CheckMenuItem)>,
    pub boost: MenuItem,
    pub pause: CheckMenuItem,
    pub exit: MenuItem,
    profile_names: Vec<String>,
    tooltip: String,
}

pub enum Action {
    Open,
    Profile(Option<String>),
    Boost,
    Pause,
    Exit,
}

impl Tray {
    pub fn build(profiles: &[String]) -> Option<Self> {
        let open = MenuItem::new("Open Hush", true, None);
        let auto = CheckMenuItem::new("Automatic", true, true, None);
        let items: Vec<(String, CheckMenuItem)> = profiles.iter().map(|p| (p.clone(), CheckMenuItem::new(p, true, false, None))).collect();
        let sub = Submenu::new("Profile", true);
        sub.append(&auto).ok()?;
        sub.append(&PredefinedMenuItem::separator()).ok()?;
        for (_, it) in &items {
            sub.append(it).ok()?;
        }
        let boost = MenuItem::new("Boost 60 s", true, None);
        let pause = CheckMenuItem::new("Pause (BIOS control)", true, false, None);
        let exit = MenuItem::new("Exit Hush", true, None);
        let menu = Menu::new();
        menu.append_items(&[&open, &PredefinedMenuItem::separator(), &sub, &boost, &pause, &PredefinedMenuItem::separator(), &exit]).ok()?;
        let icon = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Hush")
            .with_icon(tray_icon::Icon::from_rgba(logo_rgba(32), 32, 32).ok()?)
            .with_menu_on_left_click(false)
            .build()
            .ok()?;
        Some(Self { icon, open, auto, profiles: items, boost, pause, exit, profile_names: profiles.to_vec(), tooltip: String::new() })
    }

    pub fn needs_rebuild(&self, profiles: &[String]) -> bool {
        self.profile_names != profiles
    }

    pub fn action(&self, id: &MenuId) -> Option<Action> {
        if id == self.open.id() {
            return Some(Action::Open);
        }
        if id == self.auto.id() {
            return Some(Action::Profile(None));
        }
        if let Some((n, _)) = self.profiles.iter().find(|(_, it)| it.id() == id) {
            return Some(Action::Profile(Some(n.clone())));
        }
        if id == self.boost.id() {
            return Some(Action::Boost);
        }
        if id == self.pause.id() {
            return Some(Action::Pause);
        }
        if id == self.exit.id() {
            return Some(Action::Exit);
        }
        None
    }

    pub fn sync(&mut self, s: &Snapshot) {
        self.auto.set_checked(s.pinned_profile.is_none());
        for (n, it) in &self.profiles {
            it.set_checked(*n == s.profile);
        }
        self.pause.set_checked(s.paused);
        self.boost.set_text(if s.boost_left.is_some() { "Cancel boost" } else { "Boost 60 s" });
        let t = |id: &str| s.sensor(id).map(|x| format!("{:.0}°", x.value)).unwrap_or_else(|| "–".into());
        let tip = format!("Hush · {}\nCPU {}  GPU {}", s.profile, t("cpu"), t("gpu"));
        if tip != self.tooltip {
            let _ = self.icon.set_tooltip(Some(&tip));
            self.tooltip = tip;
        }
    }
}
