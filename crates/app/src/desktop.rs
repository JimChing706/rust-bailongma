use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

use anyhow::Context;
use tao::dpi::{LogicalPosition, LogicalSize};
use tao::event::{Event, WindowEvent};
use tao::event_loop::{
    ControlFlow, EventLoopBuilder, EventLoopProxy, EventLoopWindowTarget,
};
#[cfg(target_os = "windows")]
use tao::platform::windows::WindowExtWindows;
use tao::window::{Window, WindowBuilder, WindowId};
use tray_icon::menu::{Menu, MenuEvent, MenuItem, Submenu};
use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use wry::{WebView, WebViewBuilder};

use crate::{api_host, window_manager::WindowManagerState};

const SINGLE_INSTANCE_ADDR: &str = "127.0.0.1:37229";
const SINGLE_INSTANCE_SHOW_MAIN: &str = "show-main";

#[derive(Clone, Debug)]
enum DesktopUserEvent {
    TrayEvent(TrayIconEvent),
    MenuEvent(MenuEvent),
    ExternalCommand(ExternalCommand),
}

#[derive(Clone, Debug)]
enum ExternalCommand {
    ShowMainWindow,
}

pub fn run_desktop_shell() -> anyhow::Result<()> {
    let instance_listener = match acquire_single_instance()? {
        Some(listener) => listener,
        None => {
            tracing::info!("检测到已有 Bailongma Desktop 实例，已发送唤醒指令，本次启动直接退出");
            return Ok(());
        }
    };

    let server_thread = ensure_local_server(Duration::from_secs(20))?;
    tracing::info!("桌面壳正在打开原生窗口: {}", api_host::app_url());
    let user_dir = bailongma_core::config::resolve_user_dir()?;
    let mut window_manager_state = WindowManagerState::load(&user_dir).unwrap_or_else(|err| {
        tracing::warn!("读取窗口状态失败，改用默认布局: {err:#}");
        WindowManagerState::default()
    });

    let mut event_loop_builder = EventLoopBuilder::<DesktopUserEvent>::with_user_event();
    let event_loop = event_loop_builder.build();
    let tray_proxy = event_loop.create_proxy();
    TrayIconEvent::set_event_handler(Some(move |event| {
        let _ = tray_proxy.send_event(DesktopUserEvent::TrayEvent(event));
    }));
    let menu_proxy = event_loop.create_proxy();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(DesktopUserEvent::MenuEvent(event));
    }));
    let instance_listener_thread = spawn_instance_listener(instance_listener, event_loop.create_proxy());

    let main_window = build_main_window(&event_loop, &window_manager_state)
        .context("创建主窗口失败")?;
    let main_window_id = main_window.id();

    let main_webview = WebViewBuilder::new()
        .with_url(&api_host::app_url())
        .with_user_agent("BailongmaDesktop/0.1")
        .with_initialization_script(
            r#"
            window.__BAILONGMA_DESKTOP__ = true;
            document.documentElement.dataset.shell = "desktop";
            "#,
        )
        .build(&main_window)
        .context("创建主窗口 WebView 失败")?;

    let tray_toggle_item = MenuItem::with_id(
        "toggle-main-window",
        if window_manager_state.main.visible {
            "隐藏主窗口"
        } else {
            "显示主窗口"
        },
        true,
        None,
    );
    let tray_open_status_item = MenuItem::with_id("tray-open-status-window", "打开系统状态窗口", true, None);
    let tray_quit_item = MenuItem::with_id("quit-app", "退出", true, None);
    let tray_menu = Menu::new();
    tray_menu
        .append_items(&[&tray_toggle_item, &tray_open_status_item, &tray_quit_item])
        .context("创建托盘菜单失败")?;
    let tray_icon = TrayIconBuilder::new()
        .with_tooltip("Bailongma Desktop")
        .with_menu(Box::new(tray_menu.clone()))
        .with_icon(build_tray_icon()?)
        .build()
        .context("创建系统托盘失败")?;
    tracing::info!("系统托盘已创建，可通过托盘显示/隐藏主窗口、打开系统状态窗口或退出应用");

    let show_main_window_item = MenuItem::with_id("show-main-window", "显示主窗口", true, None);
    let hide_main_window_item = MenuItem::with_id("hide-main-window", "隐藏主窗口", true, None);
    let open_status_window_item =
        MenuItem::with_id("open-status-window", "打开系统状态窗口", true, None);
    let close_status_window_item =
        MenuItem::with_id(
            "close-status-window",
            "关闭系统状态窗口",
            window_manager_state.status.open,
            None,
        );
    let quit_menu_item = MenuItem::with_id("quit-app-menu", "退出应用", true, None);

    let app_submenu = Submenu::with_items(
        "应用",
        true,
        &[&show_main_window_item, &hide_main_window_item, &quit_menu_item],
    )
    .context("创建应用菜单失败")?;
    let window_submenu = Submenu::with_items(
        "窗口",
        true,
        &[&open_status_window_item, &close_status_window_item],
    )
    .context("创建窗口菜单失败")?;
    let native_menu = Menu::new();
    native_menu
        .append_items(&[&app_submenu, &window_submenu])
        .context("创建原生菜单栏失败")?;
    attach_native_menu(&native_menu, &main_window)?;
    tracing::info!("主窗口原生菜单栏已挂载");

    let mut main_window_visible = window_manager_state.main.visible;
    let mut status_window: Option<Window> = None;
    let mut status_webview: Option<WebView> = None;
    if window_manager_state.status.open {
        if let Err(err) = ensure_status_window(
            &event_loop,
            &mut status_window,
            &mut status_webview,
            &window_manager_state,
        ) {
            tracing::error!("恢复系统状态窗口失败: {err:#}");
            close_status_window_item.set_enabled(false);
        }
    }

    persist_window_manager_state(
        &user_dir,
        &mut window_manager_state,
        &main_window,
        main_window_visible,
        status_window.as_ref(),
    );

    event_loop.run(move |event, event_loop_target, control_flow| {
        let _keep_alive = (
            &server_thread,
            &instance_listener_thread,
            &main_webview,
            &tray_icon,
            &tray_menu,
            &tray_toggle_item,
            &tray_open_status_item,
            &tray_quit_item,
            &native_menu,
            &app_submenu,
            &window_submenu,
            &show_main_window_item,
            &hide_main_window_item,
            &open_status_window_item,
            &close_status_window_item,
            &quit_menu_item,
        );
        *control_flow = ControlFlow::Wait;

        match event {
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } if window_id == main_window_id => {
                tracing::info!("主窗口关闭按钮被点击，改为隐藏到系统托盘");
                set_main_window_visible(
                    &main_window,
                    &tray_toggle_item,
                    false,
                    &mut main_window_visible,
                );
                persist_window_manager_state(
                    &user_dir,
                    &mut window_manager_state,
                    &main_window,
                    main_window_visible,
                    status_window.as_ref(),
                );
            }
            Event::WindowEvent {
                window_id,
                event: WindowEvent::CloseRequested,
                ..
            } if is_status_window(window_id, &status_window) => {
                tracing::info!("系统状态窗口被关闭");
                status_window = None;
                status_webview = None;
                close_status_window_item.set_enabled(false);
                persist_window_manager_state(
                    &user_dir,
                    &mut window_manager_state,
                    &main_window,
                    main_window_visible,
                    status_window.as_ref(),
                );
            }
            Event::WindowEvent {
                window_id,
                event: WindowEvent::Moved(_) | WindowEvent::Resized(_),
                ..
            } if window_id == main_window_id => {
                persist_window_manager_state(
                    &user_dir,
                    &mut window_manager_state,
                    &main_window,
                    main_window_visible,
                    status_window.as_ref(),
                );
            }
            Event::WindowEvent {
                window_id,
                event: WindowEvent::Moved(_) | WindowEvent::Resized(_),
                ..
            } if is_status_window(window_id, &status_window) => {
                persist_window_manager_state(
                    &user_dir,
                    &mut window_manager_state,
                    &main_window,
                    main_window_visible,
                    status_window.as_ref(),
                );
            }
            Event::UserEvent(DesktopUserEvent::TrayEvent(tray_event)) => match tray_event {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                }
                | TrayIconEvent::DoubleClick {
                    button: MouseButton::Left,
                    ..
                } => {
                    tracing::info!("收到托盘左键事件，切换主窗口显示状态");
                    toggle_main_window(
                        &main_window,
                        &tray_toggle_item,
                        &mut main_window_visible,
                    );
                    persist_window_manager_state(
                        &user_dir,
                        &mut window_manager_state,
                        &main_window,
                        main_window_visible,
                        status_window.as_ref(),
                    );
                }
                _ => {}
            },
            Event::UserEvent(DesktopUserEvent::ExternalCommand(ExternalCommand::ShowMainWindow)) => {
                tracing::info!("收到单实例唤醒指令，激活主窗口");
                set_main_window_visible(
                    &main_window,
                    &tray_toggle_item,
                    true,
                    &mut main_window_visible,
                );
                persist_window_manager_state(
                    &user_dir,
                    &mut window_manager_state,
                    &main_window,
                    main_window_visible,
                    status_window.as_ref(),
                );
            }
            Event::UserEvent(DesktopUserEvent::MenuEvent(menu_event)) => {
                let menu_id = menu_event.id;
                if menu_id == tray_toggle_item.id().clone() {
                    toggle_main_window(
                        &main_window,
                        &tray_toggle_item,
                        &mut main_window_visible,
                    );
                    persist_window_manager_state(
                        &user_dir,
                        &mut window_manager_state,
                        &main_window,
                        main_window_visible,
                        status_window.as_ref(),
                    );
                } else if menu_id == tray_open_status_item.id().clone()
                    || menu_id == open_status_window_item.id().clone()
                {
                    if let Err(err) = ensure_status_window(
                        event_loop_target,
                        &mut status_window,
                        &mut status_webview,
                        &window_manager_state,
                    ) {
                        tracing::error!("打开系统状态窗口失败: {err:#}");
                    } else {
                        close_status_window_item.set_enabled(true);
                        persist_window_manager_state(
                            &user_dir,
                            &mut window_manager_state,
                            &main_window,
                            main_window_visible,
                            status_window.as_ref(),
                        );
                    }
                } else if menu_id == close_status_window_item.id().clone() {
                    tracing::info!("收到关闭系统状态窗口指令");
                    status_window = None;
                    status_webview = None;
                    close_status_window_item.set_enabled(false);
                    persist_window_manager_state(
                        &user_dir,
                        &mut window_manager_state,
                        &main_window,
                        main_window_visible,
                        status_window.as_ref(),
                    );
                } else if menu_id == show_main_window_item.id().clone() {
                    set_main_window_visible(
                        &main_window,
                        &tray_toggle_item,
                        true,
                        &mut main_window_visible,
                    );
                    persist_window_manager_state(
                        &user_dir,
                        &mut window_manager_state,
                        &main_window,
                        main_window_visible,
                        status_window.as_ref(),
                    );
                } else if menu_id == hide_main_window_item.id().clone() {
                    set_main_window_visible(
                        &main_window,
                        &tray_toggle_item,
                        false,
                        &mut main_window_visible,
                    );
                    persist_window_manager_state(
                        &user_dir,
                        &mut window_manager_state,
                        &main_window,
                        main_window_visible,
                        status_window.as_ref(),
                    );
                } else if menu_id == tray_quit_item.id().clone()
                    || menu_id == quit_menu_item.id().clone()
                {
                    tracing::info!("收到退出指令，正在退出 Bailongma Desktop");
                    persist_window_manager_state(
                        &user_dir,
                        &mut window_manager_state,
                        &main_window,
                        main_window_visible,
                        status_window.as_ref(),
                    );
                    TrayIconEvent::set_event_handler::<fn(TrayIconEvent)>(None);
                    MenuEvent::set_event_handler::<fn(MenuEvent)>(None);
                    *control_flow = ControlFlow::Exit;
                }
            }
            _ => {}
        }
    });
}

fn is_status_window(window_id: WindowId, status_window: &Option<Window>) -> bool {
    status_window
        .as_ref()
        .map(|window| window.id() == window_id)
        .unwrap_or(false)
}

fn build_main_window(
    event_loop_target: &EventLoopWindowTarget<DesktopUserEvent>,
    window_manager_state: &WindowManagerState,
) -> anyhow::Result<Window> {
    let main_state = &window_manager_state.main;
    let mut builder = WindowBuilder::new()
        .with_title("Bailongma Desktop")
        .with_inner_size(LogicalSize::new(main_state.width, main_state.height))
        .with_min_inner_size(LogicalSize::new(1100.0, 720.0))
        .with_resizable(true)
        .with_visible(main_state.visible);

    if let (Some(x), Some(y)) = (main_state.x, main_state.y) {
        builder = builder.with_position(LogicalPosition::new(x, y));
    }

    let window = builder.build(event_loop_target)?;
    if main_state.maximized {
        window.set_maximized(true);
    }
    Ok(window)
}

fn ensure_status_window(
    event_loop_target: &EventLoopWindowTarget<DesktopUserEvent>,
    status_window: &mut Option<Window>,
    status_webview: &mut Option<WebView>,
    window_manager_state: &WindowManagerState,
) -> anyhow::Result<()> {
    if let Some(window) = status_window.as_ref() {
        window.set_minimized(false);
        window.set_visible(true);
        window.set_focus();
        tracing::info!("系统状态窗口已存在，直接激活");
        return Ok(());
    }

    tracing::info!("正在创建系统状态窗口");
    let status_state = &window_manager_state.status;
    let mut builder = WindowBuilder::new()
        .with_title("Bailongma System Status")
        .with_inner_size(LogicalSize::new(status_state.width, status_state.height))
        .with_min_inner_size(LogicalSize::new(760.0, 560.0))
        .with_resizable(true);

    if let (Some(x), Some(y)) = (status_state.x, status_state.y) {
        builder = builder.with_position(LogicalPosition::new(x, y));
    }

    let window = builder
        .build(event_loop_target)
        .context("创建系统状态窗口失败")?;
    if status_state.maximized {
        window.set_maximized(true);
    }

    let webview = WebViewBuilder::new()
        .with_html(build_status_window_html())
        .with_user_agent("BailongmaDesktop/0.1")
        .build(&window)
        .context("创建系统状态窗口 WebView 失败")?;

    *status_window = Some(window);
    *status_webview = Some(webview);
    tracing::info!("系统状态窗口已创建");
    Ok(())
}

fn acquire_single_instance() -> anyhow::Result<Option<TcpListener>> {
    match TcpListener::bind(SINGLE_INSTANCE_ADDR) {
        Ok(listener) => {
            listener
                .set_nonblocking(true)
                .context("设置单实例监听为非阻塞模式失败")?;
            Ok(Some(listener))
        }
        Err(err) if err.kind() == std::io::ErrorKind::AddrInUse => {
            signal_existing_instance().ok();
            Ok(None)
        }
        Err(err) => Err(err).context("创建单实例监听失败"),
    }
}

fn signal_existing_instance() -> anyhow::Result<()> {
    let mut stream =
        TcpStream::connect(SINGLE_INSTANCE_ADDR).context("连接现有实例失败")?;
    stream
        .write_all(SINGLE_INSTANCE_SHOW_MAIN.as_bytes())
        .context("发送唤醒指令失败")?;
    Ok(())
}

fn spawn_instance_listener(
    listener: TcpListener,
    proxy: EventLoopProxy<DesktopUserEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("bailongma-instance-listener".into())
        .spawn(move || loop {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let mut buffer = [0u8; 64];
                    let read = stream.read(&mut buffer).unwrap_or(0);
                    let command = String::from_utf8_lossy(&buffer[..read]);
                    if command.contains(SINGLE_INSTANCE_SHOW_MAIN) {
                        let _ = proxy.send_event(DesktopUserEvent::ExternalCommand(
                            ExternalCommand::ShowMainWindow,
                        ));
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(250));
                }
                Err(err) => {
                    tracing::warn!("单实例监听线程退出: {err}");
                    break;
                }
            }
        })
        .expect("启动单实例监听线程失败")
}

fn attach_native_menu(menu: &Menu, window: &Window) -> anyhow::Result<()> {
    #[cfg(target_os = "windows")]
    unsafe {
        menu.init_for_hwnd(window.hwnd() as _)
            .context("绑定主窗口原生菜单失败")?;
    }

    #[cfg(target_os = "macos")]
    menu.init_for_nsapp().context("绑定 macOS 应用菜单失败")?;

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let _ = (menu, window);

    Ok(())
}

fn toggle_main_window(window: &Window, tray_toggle_item: &MenuItem, window_visible: &mut bool) {
    let next_visible = !*window_visible;
    set_main_window_visible(window, tray_toggle_item, next_visible, window_visible);
}

fn set_main_window_visible(
    window: &Window,
    tray_toggle_item: &MenuItem,
    visible: bool,
    window_visible: &mut bool,
) {
    if visible {
        window.set_minimized(false);
        window.set_visible(true);
        window.set_focus();
        tray_toggle_item.set_text("隐藏主窗口");
        tracing::info!("主窗口已显示");
    } else {
        window.set_visible(false);
        tray_toggle_item.set_text("显示主窗口");
        tracing::info!("主窗口已隐藏到系统托盘");
    }
    *window_visible = visible;
}

fn persist_window_manager_state(
    user_dir: &Path,
    window_manager_state: &mut WindowManagerState,
    main_window: &Window,
    main_window_visible: bool,
    status_window: Option<&Window>,
) {
    window_manager_state.capture_main(main_window, main_window_visible);
    window_manager_state.capture_status(status_window);
    if let Err(err) = window_manager_state.save(user_dir) {
        tracing::warn!("保存窗口管理状态失败: {err:#}");
    }
}

fn build_status_window_html() -> String {
    format!(
        r#"<!doctype html>
<html lang="zh-CN">
<head>
  <meta charset="UTF-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1.0" />
  <title>Bailongma System Status</title>
  <style>
    body {{
      margin: 0;
      font-family: "Segoe UI", "PingFang SC", "Microsoft YaHei", sans-serif;
      color: #eef4ff;
      background: linear-gradient(180deg, #0b1120, #10192d 60%, #0b1324);
    }}
    .wrap {{
      max-width: 920px;
      margin: 0 auto;
      padding: 28px 24px 40px;
    }}
    h1 {{
      margin: 0 0 10px;
      font-size: 30px;
    }}
    .sub {{
      color: #9ab0d2;
      line-height: 1.7;
      margin-bottom: 22px;
    }}
    .grid {{
      display: grid;
      grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
      gap: 14px;
      margin-bottom: 20px;
    }}
    .card {{
      border: 1px solid rgba(255,255,255,.08);
      border-radius: 18px;
      padding: 18px;
      background: rgba(255,255,255,.03);
      box-shadow: 0 12px 36px rgba(0,0,0,.24);
    }}
    .label {{
      color: #8ea5ca;
      font-size: 13px;
      margin-bottom: 10px;
    }}
    .value {{
      font-size: 26px;
      font-weight: 700;
      word-break: break-word;
    }}
    .panel {{
      border: 1px solid rgba(255,255,255,.08);
      border-radius: 18px;
      padding: 18px;
      background: rgba(255,255,255,.03);
      margin-bottom: 14px;
    }}
    .links {{
      display: flex;
      gap: 10px;
      flex-wrap: wrap;
      margin-top: 10px;
    }}
    a {{
      color: #b8d0ff;
      text-decoration: none;
      padding: 8px 12px;
      border-radius: 999px;
      border: 1px solid rgba(255,255,255,.08);
      background: rgba(255,255,255,.03);
    }}
    code, pre {{
      font-family: Consolas, "Courier New", monospace;
    }}
    pre {{
      margin: 0;
      padding: 14px;
      white-space: pre-wrap;
      word-break: break-word;
      border-radius: 14px;
      background: rgba(0,0,0,.28);
      color: #d8e3f8;
      max-height: 280px;
      overflow: auto;
    }}
  </style>
</head>
<body>
  <div class="wrap">
    <h1>Bailongma System Status</h1>
    <div class="sub">
      这是桌面版的第二个原生窗口，用来展示系统状态、服务在线情况和桌面壳入口。
      它和主窗口独立管理，说明当前桌面版已经进入多窗口结构。
    </div>

    <div class="grid">
      <div class="card">
        <div class="label">主窗口</div>
        <div class="value">Bailongma Desktop</div>
      </div>
      <div class="card">
        <div class="label">服务地址</div>
        <div class="value">{root_url}</div>
      </div>
      <div class="card">
        <div class="label">状态接口</div>
        <div class="value" id="statusText">加载中...</div>
      </div>
    </div>

    <div class="panel">
      <div class="label">桌面结构</div>
      <pre id="detailText">正在加载服务状态...</pre>
    </div>

    <div class="panel">
      <div class="label">快捷入口</div>
      <div class="links">
        <a href="{root_url}" target="_blank">打开首页控制台</a>
        <a href="{status_url}" target="_blank">打开状态接口</a>
        <a href="{events_url}" target="_blank">打开 SSE 事件流</a>
      </div>
    </div>
  </div>

  <script>
    const statusText = document.getElementById('statusText');
    const detailText = document.getElementById('detailText');

    async function loadStatus() {{
      try {{
        const res = await fetch('{status_url}');
        const json = await res.json();
        statusText.textContent = json.running ? '运行中' : '未运行';
        detailText.textContent = JSON.stringify({{
          desktopShell: true,
          multiWindow: true,
          rootUrl: '{root_url}',
          status: json
        }}, null, 2);
      }} catch (error) {{
        statusText.textContent = '加载失败';
        detailText.textContent = String(error);
      }}
    }}

    loadStatus();
    setInterval(loadStatus, 5000);
  </script>
</body>
</html>"#,
        root_url = api_host::app_url(),
        status_url = api_host::status_url(),
        events_url = format!("http://127.0.0.1:3721/events"),
    )
}

fn build_tray_icon() -> anyhow::Result<Icon> {
    let width = 32;
    let height = 32;
    let mut rgba = Vec::with_capacity((width * height * 4) as usize);

    for y in 0..height {
        for x in 0..width {
            let border = x < 2 || x >= width - 2 || y < 2 || y >= height - 2;
            let (r, g, b, a) = if border {
                (118, 168, 255, 255)
            } else if x < width / 2 {
                (40, 89, 201, 255)
            } else {
                (123, 98, 255, 255)
            };
            rgba.extend_from_slice(&[r, g, b, a]);
        }
    }

    Icon::from_rgba(rgba, width, height).context("生成托盘图标失败")
}

fn ensure_local_server(timeout: Duration) -> anyhow::Result<Option<std::thread::JoinHandle<()>>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("创建桌面壳等待运行时失败")?;

    if runtime.block_on(api_host::is_local_server_ready()) {
        tracing::info!("检测到本地 API 服务已在运行，桌面壳将直接复用");
        return Ok(None);
    }

    tracing::info!("未检测到本地 API 服务，正在以内嵌线程启动");
    let handle = std::thread::Builder::new()
        .name("bailongma-api".into())
        .spawn(|| {
            let runtime = match tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(err) => {
                    eprintln!("[fatal] 创建 API 运行时失败: {err}");
                    return;
                }
            };

            if let Err(err) = runtime.block_on(api_host::run_api_server()) {
                eprintln!("[fatal] 桌面壳内嵌 API 服务退出: {err}");
            }
        })
        .context("启动内嵌 API 线程失败")?;

    runtime
        .block_on(api_host::wait_until_ready(timeout))
        .context("等待内嵌 API 服务就绪失败")?;

    Ok(Some(handle))
}
