//! 托盘常驻入口(macOS)。
//!
//! 托盘本身只是入口:服务在同进程的后台线程里跑,菜单提供打开控制台、
//! 切换模式、启停 Science。真正的配置面在 WebUI。

#![cfg(target_os = "macos")]

use std::sync::Arc;

use muda::{Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tao::event_loop::{ControlFlow, EventLoopBuilder};
use tray_icon::{TrayIconBuilder, TrayIconEvent};

use crate::profile::{Mode, Profile};

/// 托盘图标:一个纯色圆点,避免依赖外部资源文件。
fn icon() -> tray_icon::Icon {
    const SIZE: u32 = 22;
    let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
    let center = (SIZE as f32 - 1.0) / 2.0;
    let radius = center - 1.5;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = x as f32 - center;
            let dy = y as f32 - center;
            let distance = (dx * dx + dy * dy).sqrt();
            // 边缘一像素做线性淡出,免得图标看起来是锯齿方块。
            let alpha = ((radius - distance + 0.5).clamp(0.0, 1.0) * 255.0) as u8;
            rgba.extend_from_slice(&[0xc9, 0x64, 0x42, alpha]);
        }
    }
    tray_icon::Icon::from_rgba(rgba, SIZE, SIZE).expect("托盘图标构造失败")
}

fn open_browser(url: &str) {
    let _ = std::process::Command::new("open").arg(url).spawn();
}

pub fn run(port_override: Option<u16>) -> Result<(), String> {
    let mut profile = Profile::load();
    if let Some(port) = port_override {
        profile.port = port;
    }
    let port = profile.port;
    let console = format!("http://127.0.0.1:{port}/");

    // 服务在后台线程,托盘占用主线程的事件循环(macOS 要求)。
    {
        let port = Some(port);
        std::thread::spawn(move || {
            if let Err(error) = crate::control::serve(port) {
                eprintln!("csswitch service: {error}");
                std::process::exit(1);
            }
        });
    }

    let event_loop = EventLoopBuilder::new().build();
    let menu = Menu::new();
    let open = MenuItem::new("打开控制台", true, None);
    let modes = Submenu::new("切换模式", true);
    let official = MenuItem::new(Mode::Official.display_name(), true, None);
    let kimi = MenuItem::new(Mode::Kimi.display_name(), true, None);
    let deepseek = MenuItem::new(Mode::Deepseek.display_name(), true, None);
    modes
        .append_items(&[&official, &kimi, &deepseek])
        .map_err(|e| e.to_string())?;
    let science_open = MenuItem::new("打开 Science", true, None);
    let science_stop = MenuItem::new("停止 Science", true, None);
    let quit = MenuItem::new("退出 CSSwitch", true, None);
    menu.append_items(&[
        &open,
        &PredefinedMenuItem::separator(),
        &modes,
        &PredefinedMenuItem::separator(),
        &science_open,
        &science_stop,
        &PredefinedMenuItem::separator(),
        &quit,
    ])
    .map_err(|e| e.to_string())?;

    let _tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(format!("CSSwitch · {}", profile.mode.display_name()))
        .with_icon(icon())
        .build()
        .map_err(|e| format!("托盘初始化失败:{e}"))?;

    let menu_channel = MenuEvent::receiver();
    let tray_channel = TrayIconEvent::receiver();
    let ids = Arc::new((
        open.id().clone(),
        official.id().clone(),
        kimi.id().clone(),
        deepseek.id().clone(),
        science_open.id().clone(),
        science_stop.id().clone(),
        quit.id().clone(),
    ));

    event_loop.run(move |_event, _target, control_flow| {
        *control_flow = ControlFlow::Wait;
        let _ = tray_channel.try_recv();
        let Ok(event) = menu_channel.try_recv() else {
            return;
        };
        let (open_id, official_id, kimi_id, deepseek_id, sci_open_id, sci_stop_id, quit_id) = &*ids;
        if event.id == *open_id {
            open_browser(&console);
        } else if event.id == *quit_id {
            *control_flow = ControlFlow::Exit;
        } else if event.id == *sci_stop_id {
            if let Err(error) = crate::science::stop() {
                eprintln!("停止 Science 失败:{error}");
            }
        } else if event.id == *sci_open_id {
            match crate::science::login_url() {
                Ok(url) => open_browser(&url),
                // 托盘没有展示错误的位置,引导用户去控制台看具体原因。
                Err(error) => {
                    eprintln!("获取 Science 链接失败:{error}");
                    open_browser(&console);
                }
            }
        } else {
            let mode = if event.id == *official_id {
                Some(Mode::Official)
            } else if event.id == *kimi_id {
                Some(Mode::Kimi)
            } else if event.id == *deepseek_id {
                Some(Mode::Deepseek)
            } else {
                None
            };
            if let Some(mode) = mode {
                // 切换要重启 daemon,走控制 API 复用同一套校验与重启逻辑。
                let url = format!("http://127.0.0.1:{port}/control/switch");
                let body = format!("{{\"mode\":\"{}\"}}", mode.as_str());
                std::thread::spawn(move || {
                    let client = reqwest::blocking::Client::new();
                    match client
                        .post(&url)
                        .header("content-type", "application/json")
                        .body(body)
                        .send()
                    {
                        Ok(response) if response.status().is_success() => {}
                        Ok(response) => {
                            eprintln!("切换失败:{}", response.text().unwrap_or_default())
                        }
                        Err(error) => eprintln!("切换失败:{error}"),
                    }
                });
            }
        }
    });
}
