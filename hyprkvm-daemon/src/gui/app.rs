//! Main Iced Application implementation

use iced::widget::{button, column, container, row, text, text_input, Space};
use iced::{Element, Length, Task};
use std::path::PathBuf;

use super::canvas::machine_canvas;
use super::messages::{CanvasEvent, Message};
use super::state::GuiState;
use super::theme;
use crate::config::Config;

/// The main HyprKVM GUI application
pub struct HyprKvmGui {
    state: GuiState,
}

impl HyprKvmGui {
    /// Create a new GUI from a config path
    pub fn new(config_path: PathBuf) -> (Self, Task<Message>) {
        // Load config or use default
        let config = Config::load(&config_path).unwrap_or_else(|e| {
            tracing::warn!("Failed to load config: {}, using defaults", e);
            Config::default()
        });

        let state = GuiState::from_config(config_path, config);

        (Self { state }, Task::none())
    }

    /// Update state based on message
    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::CanvasEvent(event) => self.handle_canvas_event(event),
            Message::AddMachine => {
                self.state.show_add_modal = true;
                self.state.new_machine = Default::default();
                Task::none()
            }
            Message::CancelAddMachine => {
                self.state.show_add_modal = false;
                Task::none()
            }
            Message::ConfirmAddMachine => {
                self.confirm_add_machine();
                Task::none()
            }
            Message::RemoveMachine(idx) => {
                if self.state.remove_machine(idx) {
                    self.save_and_reload()
                } else {
                    Task::none()
                }
            }
            Message::UpdateNewMachineName(name) => {
                self.state.new_machine.name = name;
                Task::none()
            }
            Message::UpdateNewMachineAddress(address) => {
                self.state.new_machine.address = address;
                Task::none()
            }
            Message::SelectNewMachinePosition(pos) => {
                // Add machine at the selected position
                let name = self.state.new_machine.name.clone();
                let address = self.state.new_machine.address.clone();
                match self.state.add_machine(name, address, pos) {
                    Ok(()) => {
                        self.state.show_add_modal = false;
                        self.save_and_reload()
                    }
                    Err(e) => {
                        self.state.error = Some(e);
                        Task::none()
                    }
                }
            }
            Message::SaveConfig => self.save_and_reload(),
            Message::ReloadDaemon => {
                self.reload_daemon();
                Task::none()
            }
            Message::RestartDaemon => {
                self.restart_daemon();
                Task::none()
            }
            Message::ConfigSaved(result) => {
                if let Err(e) = result {
                    self.state.error = Some(format!("Failed to save config: {}", e));
                }
                Task::none()
            }
            Message::DaemonReloaded(result) => {
                if let Err(e) = result {
                    self.state.error = Some(format!("Failed to reload daemon: {}", e));
                }
                Task::none()
            }
            Message::StatusUpdate(statuses) => {
                for (name, status) in statuses {
                    for machine in &mut self.state.machines {
                        if machine.name == name {
                            machine.status = status;
                        }
                    }
                }
                Task::none()
            }
            Message::ClearError => {
                self.state.error = None;
                Task::none()
            }
            Message::ClearSuccess => {
                self.state.success = None;
                Task::none()
            }
        }
    }

    fn handle_canvas_event(&mut self, event: CanvasEvent) -> Task<Message> {
        match event {
            CanvasEvent::MachinePressed(idx) => {
                // Don't drag self machine
                if !self.state.machines[idx].is_self {
                    self.state.drag.start(idx);
                }
                Task::none()
            }
            CanvasEvent::MouseMoved(pos, snap_target) => {
                self.state.drag.update(pos);
                self.state.drag.snap_target = snap_target;
                Task::none()
            }
            CanvasEvent::MouseReleased => {
                if let Some((idx, snap_target)) = self.state.drag.end() {
                    if let Some(new_pos) = snap_target {
                        if self.state.move_machine(idx, new_pos) {
                            return self.save_and_reload();
                        }
                    }
                }
                Task::none()
            }
            CanvasEvent::MouseEntered | CanvasEvent::MouseExited => Task::none(),
        }
    }

    fn confirm_add_machine(&mut self) {
        // Find first available position
        let available = self.state.available_positions();
        if available.is_empty() {
            self.state.error = Some("No available positions".to_string());
            return;
        }

        let name = self.state.new_machine.name.clone();
        let address = self.state.new_machine.address.clone();

        if name.is_empty() || address.is_empty() {
            self.state.error = Some("Name and address are required".to_string());
            return;
        }

        // Use first available position
        let pos = available[0];

        match self.state.add_machine(name, address, pos) {
            Ok(()) => {
                self.state.show_add_modal = false;
            }
            Err(e) => {
                self.state.error = Some(e);
            }
        }
    }

    fn save_and_reload(&mut self) -> Task<Message> {
        // Save config
        if let Err(e) = self.state.save_config() {
            self.state.error = Some(format!("Failed to save: {}", e));
            return Task::none();
        }

        // Reload daemon via IPC
        self.reload_daemon();
        Task::none()
    }

    fn reload_daemon(&mut self) {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;

        let socket_path = crate::ipc::socket_path();

        match UnixStream::connect(&socket_path) {
            Ok(mut stream) => {
                // Send reload request (format: {"type": "reload"})
                if writeln!(stream, r#"{{"type":"reload"}}"#).is_err() {
                    self.state.error = Some("Failed to send reload".to_string());
                    return;
                }
                stream.flush().ok();

                // Set read timeout
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .ok();

                // Read response
                let mut reader = BufReader::new(&stream);
                let mut response = String::new();
                if reader.read_line(&mut response).is_ok() {
                    if response.contains("Error") {
                        self.state.error = Some("Failed to reload daemon".to_string());
                        self.state.needs_restart = false;
                    } else if response.contains("restart required") {
                        self.state.error = Some("Direction changed - restart required".to_string());
                        self.state.needs_restart = true;
                    } else {
                        self.state.success = Some("Config saved & applied".to_string());
                        self.state.needs_restart = false;
                    }
                } else {
                    self.state.success = Some("Config saved".to_string());
                    self.state.needs_restart = false;
                }
            }
            Err(_) => {
                // Daemon not running - just show config saved
                self.state.success = Some("Config saved (daemon not running)".to_string());
                self.state.needs_restart = false;
            }
        }
    }

    fn restart_daemon(&mut self) {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixStream;
        use std::process::Command;

        let socket_path = crate::ipc::socket_path();

        // First, try to shutdown the running daemon
        if let Ok(mut stream) = UnixStream::connect(&socket_path) {
            // Send shutdown request
            if writeln!(stream, r#"{{"type":"shutdown"}}"#).is_ok() {
                stream.flush().ok();
                // Wait for response
                stream.set_read_timeout(Some(std::time::Duration::from_secs(2))).ok();
                let mut reader = BufReader::new(&stream);
                let mut response = String::new();
                let _ = reader.read_line(&mut response);
            }
            // Give daemon time to shutdown
            std::thread::sleep(std::time::Duration::from_millis(500));
        }

        // Now spawn a new daemon process
        let exe = std::env::current_exe().unwrap_or_else(|_| "hyprkvm".into());
        match Command::new(&exe)
            .arg("daemon")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            Ok(_) => {
                // Wait a moment for daemon to start
                std::thread::sleep(std::time::Duration::from_millis(500));
                self.state.error = None;
                self.state.needs_restart = false;
                self.state.success = Some("Daemon restarted".to_string());
            }
            Err(e) => {
                self.state.error = Some(format!("Failed to start daemon: {}", e));
                self.state.needs_restart = false;
            }
        }
    }

    /// Build the view
    pub fn view(&self) -> Element<'_, Message> {
        // Toolbar
        let toolbar = self.toolbar_view();

        // Main canvas
        let canvas = machine_canvas(
            self.state.machines.clone(),
            self.state.drag.clone(),
            self.state.available_positions(),
        );

        // Notification banners
        let error_banner = self.error_banner();
        let success_banner = self.success_banner();

        // Add machine modal (if showing)
        let content: Element<Message> = if self.state.show_add_modal {
            let modal = self.add_machine_modal();
            // Overlay modal on canvas
            container(
                column![toolbar, error_banner, success_banner, container(modal).center_x(Length::Fill).center_y(Length::Fill)]
                    .spacing(0),
            )
            .width(Length::Fill)
            .height(Length::Fill)
            .into()
        } else {
            column![toolbar, error_banner, success_banner, canvas]
                .spacing(0)
                .width(Length::Fill)
                .height(Length::Fill)
                .into()
        };

        container(content)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(theme::BACKGROUND.into()),
                ..Default::default()
            })
            .into()
    }

    fn toolbar_view(&self) -> Element<'_, Message> {
        let add_btn = button(text("+").size(20))
            .padding([5, 15])
            .on_press(Message::AddMachine);

        let title = text("HyprKVM").size(18);

        let save_btn = button(text("Save"))
            .padding([5, 10])
            .on_press(Message::SaveConfig);

        container(
            row![add_btn, Space::with_width(Length::Fill), title, Space::with_width(Length::Fill), save_btn]
                .spacing(10)
                .padding(10),
        )
        .width(Length::Fill)
        .style(|_theme| container::Style {
            background: Some(iced::Color::from_rgb(0.95, 0.93, 0.91).into()),
            ..Default::default()
        })
        .into()
    }

    fn error_banner(&self) -> Element<'_, Message> {
        if let Some(ref error) = self.state.error {
            let mut row_content = row![
                text(error).style(|_theme| text::Style {
                    color: Some(theme::STATUS_DISCONNECTED)
                }),
                Space::with_width(Length::Fill)
            ];

            // Add restart button if restart is needed
            if self.state.needs_restart {
                row_content = row_content.push(
                    button(text("Restart Daemon"))
                        .padding([5, 10])
                        .on_press(Message::RestartDaemon)
                );
                row_content = row_content.push(Space::with_width(10));
            }

            row_content = row_content.push(button(text("x")).on_press(Message::ClearError));

            container(row_content.padding(10))
                .width(Length::Fill)
                .style(|_theme| container::Style {
                    background: Some(iced::Color::from_rgba(1.0, 0.9, 0.9, 1.0).into()),
                    ..Default::default()
                })
                .into()
        } else {
            Space::with_height(0).into()
        }
    }

    fn success_banner(&self) -> Element<'_, Message> {
        if let Some(ref msg) = self.state.success {
            container(
                row![
                    text(msg).style(|_theme| text::Style {
                        color: Some(theme::STATUS_CONNECTED)
                    }),
                    Space::with_width(Length::Fill),
                    button(text("x")).on_press(Message::ClearSuccess)
                ]
                .padding(10),
            )
            .width(Length::Fill)
            .style(|_theme| container::Style {
                background: Some(iced::Color::from_rgba(0.9, 1.0, 0.9, 1.0).into()),
                ..Default::default()
            })
            .into()
        } else {
            Space::with_height(0).into()
        }
    }

    fn add_machine_modal(&self) -> Element<'_, Message> {
        let available = self.state.available_positions();

        // Position buttons
        let position_buttons: Vec<Element<Message>> = available
            .iter()
            .map(|&pos| {
                let label = pos.to_direction().map(|d| format!("{}", d)).unwrap_or_default();
                button(text(label))
                    .padding([8, 16])
                    .on_press(Message::SelectNewMachinePosition(pos))
                    .into()
            })
            .collect();

        let position_row = row(position_buttons).spacing(10);

        container(
            column![
                text("Add Machine").size(20),
                Space::with_height(20),
                text("Name:"),
                text_input("Machine name", &self.state.new_machine.name)
                    .on_input(Message::UpdateNewMachineName)
                    .padding(8),
                Space::with_height(10),
                text("Address:"),
                text_input("192.168.1.100:24850", &self.state.new_machine.address)
                    .on_input(Message::UpdateNewMachineAddress)
                    .padding(8),
                Space::with_height(20),
                text("Position:"),
                position_row,
                Space::with_height(20),
                row![
                    button(text("Cancel")).on_press(Message::CancelAddMachine),
                    Space::with_width(10),
                    button(text("Add")).on_press(Message::ConfirmAddMachine)
                ]
            ]
            .spacing(5)
            .padding(30),
        )
        .style(|_theme| container::Style {
            background: Some(iced::Color::WHITE.into()),
            border: iced::Border {
                color: iced::Color::from_rgb(0.8, 0.8, 0.8),
                width: 1.0,
                radius: 8.0.into(),
            },
            shadow: iced::Shadow {
                color: iced::Color::from_rgba(0.0, 0.0, 0.0, 0.2),
                offset: iced::Vector::new(0.0, 2.0),
                blur_radius: 10.0,
            },
            ..Default::default()
        })
        .width(Length::Fixed(400.0))
        .into()
    }

    /// Window title
    pub fn title(&self) -> String {
        format!("HyprKVM - {}", self.state.config.machines.self_name)
    }

    /// Theme
    pub fn theme(&self) -> iced::Theme {
        iced::Theme::Light
    }
}
