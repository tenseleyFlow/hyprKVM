//! Canvas rendering for machine grid

use iced::widget::canvas::{self, Cache, Canvas, Event, Frame, Geometry, Path, Stroke, Text};
use iced::mouse;
use iced::{Color, Point, Rectangle, Renderer, Size, Theme};

use super::messages::{CanvasEvent, Message};
use super::state::{ConnectionStatus, DragState, GridPos, MachineNode};
use super::theme;

/// The machine grid canvas program
pub struct MachineGrid {
    /// All machines to render
    machines: Vec<MachineNode>,
    /// Current drag state
    drag: DragState,
    /// Available snap positions
    available_positions: Vec<GridPos>,
    /// Render cache
    cache: Cache,
}

impl MachineGrid {
    pub fn new(
        machines: Vec<MachineNode>,
        drag: DragState,
        available_positions: Vec<GridPos>,
    ) -> Self {
        Self {
            machines,
            drag,
            available_positions,
            cache: Cache::new(),
        }
    }

    /// Convert grid position to canvas coordinates
    fn grid_to_canvas(&self, pos: GridPos, bounds: &Rectangle) -> Point {
        let center = Point::new(bounds.width / 2.0, bounds.height / 2.0);
        Point::new(
            center.x + (pos.x as f32 * theme::GRID_SPACING_H),
            center.y + (pos.y as f32 * theme::GRID_SPACING_V),
        )
    }

    /// Convert canvas coordinates to nearest grid position
    fn canvas_to_grid(&self, point: Point, bounds: &Rectangle) -> GridPos {
        let center = Point::new(bounds.width / 2.0, bounds.height / 2.0);
        let relative = Point::new(point.x - center.x, point.y - center.y);

        GridPos {
            x: (relative.x / theme::GRID_SPACING_H).round() as i32,
            y: (relative.y / theme::GRID_SPACING_V).round() as i32,
        }
    }

    /// Find machine at canvas position
    fn machine_at_position(&self, point: Point, bounds: &Rectangle) -> Option<usize> {
        for (idx, machine) in self.machines.iter().enumerate() {
            let machine_center = self.grid_to_canvas(machine.grid_pos, bounds);
            let rect = Rectangle::new(
                Point::new(
                    machine_center.x - theme::MACHINE_WIDTH / 2.0,
                    machine_center.y - theme::MACHINE_HEIGHT / 2.0,
                ),
                Size::new(theme::MACHINE_WIDTH, theme::MACHINE_HEIGHT),
            );

            if rect.contains(point) {
                return Some(idx);
            }
        }
        None
    }

    /// Find snap target near cursor position
    fn find_snap_target(&self, point: Point, bounds: &Rectangle) -> Option<GridPos> {
        for &pos in &self.available_positions {
            let target = self.grid_to_canvas(pos, bounds);
            let distance = ((point.x - target.x).powi(2) + (point.y - target.y).powi(2)).sqrt();

            if distance < theme::SNAP_RADIUS {
                return Some(pos);
            }
        }
        None
    }

    /// Draw a machine rectangle
    fn draw_machine(&self, frame: &mut Frame, machine: &MachineNode, center: Point, dragging: bool) {
        let half_w = theme::MACHINE_WIDTH / 2.0;
        let half_h = theme::MACHINE_HEIGHT / 2.0;

        // Machine background
        let bg_color = if machine.is_self {
            theme::MACHINE_SELF
        } else {
            theme::MACHINE_NEIGHBOR
        };

        // Apply opacity if dragging
        let bg_color = if dragging {
            Color::from_rgba(bg_color.r, bg_color.g, bg_color.b, 0.6)
        } else {
            bg_color
        };

        let rect = Path::rectangle(
            Point::new(center.x - half_w, center.y - half_h),
            Size::new(theme::MACHINE_WIDTH, theme::MACHINE_HEIGHT),
        );

        frame.fill(&rect, bg_color);

        // Status indicator (small circle in corner)
        if !machine.is_self {
            let status_color = match machine.status {
                ConnectionStatus::Connected => theme::STATUS_CONNECTED,
                ConnectionStatus::Connecting => theme::STATUS_CONNECTING,
                ConnectionStatus::Disconnected => theme::STATUS_DISCONNECTED,
            };

            let status_pos = Point::new(center.x + half_w - 12.0, center.y - half_h + 12.0);
            let status_circle = Path::circle(status_pos, 6.0);
            frame.fill(&status_circle, status_color);
        }

        // Machine name
        let name_text = Text {
            content: machine.name.clone(),
            position: Point::new(center.x, center.y - 10.0),
            color: theme::TEXT_LIGHT,
            size: iced::Pixels(16.0),
            horizontal_alignment: iced::alignment::Horizontal::Center,
            vertical_alignment: iced::alignment::Vertical::Center,
            ..Default::default()
        };
        frame.fill_text(name_text);

        // Address (for neighbors)
        if !machine.is_self && !machine.address.is_empty() {
            let addr_text = Text {
                content: machine.address.clone(),
                position: Point::new(center.x, center.y + 15.0),
                color: Color::from_rgba(1.0, 1.0, 1.0, 0.7),
                size: iced::Pixels(11.0),
                horizontal_alignment: iced::alignment::Horizontal::Center,
                vertical_alignment: iced::alignment::Vertical::Center,
                ..Default::default()
            };
            frame.fill_text(addr_text);
        }

        // Self label
        if machine.is_self {
            let self_text = Text {
                content: "(this machine)".to_string(),
                position: Point::new(center.x, center.y + 15.0),
                color: Color::from_rgba(1.0, 1.0, 1.0, 0.7),
                size: iced::Pixels(11.0),
                horizontal_alignment: iced::alignment::Horizontal::Center,
                vertical_alignment: iced::alignment::Vertical::Center,
                ..Default::default()
            };
            frame.fill_text(self_text);
        }
    }

    /// Draw a snap target indicator
    fn draw_snap_target(&self, frame: &mut Frame, center: Point, highlighted: bool) {
        let half_w = theme::MACHINE_WIDTH / 2.0;
        let half_h = theme::MACHINE_HEIGHT / 2.0;

        let color = if highlighted {
            theme::SNAP_HIGHLIGHT
        } else {
            Color::from_rgba(0.5, 0.5, 0.5, 0.15)
        };

        let rect = Path::rectangle(
            Point::new(center.x - half_w, center.y - half_h),
            Size::new(theme::MACHINE_WIDTH, theme::MACHINE_HEIGHT),
        );

        frame.fill(&rect, color);

        // Dashed border
        let stroke = Stroke::default()
            .with_color(Color::from_rgba(0.5, 0.5, 0.5, 0.4))
            .with_width(2.0);

        frame.stroke(&rect, stroke);

        // Direction label
        if let Some(direction) = (GridPos {
            x: ((center.x - frame.size().width / 2.0) / theme::GRID_SPACING_H).round() as i32,
            y: ((center.y - frame.size().height / 2.0) / theme::GRID_SPACING_V).round() as i32,
        })
        .to_direction()
        {
            let label = Text {
                content: format!("{}", direction),
                position: center,
                color: Color::from_rgba(0.4, 0.4, 0.4, 0.6),
                size: iced::Pixels(14.0),
                horizontal_alignment: iced::alignment::Horizontal::Center,
                vertical_alignment: iced::alignment::Vertical::Center,
                ..Default::default()
            };
            frame.fill_text(label);
        }
    }
}

impl canvas::Program<Message> for MachineGrid {
    type State = ();

    fn update(
        &self,
        _state: &mut Self::State,
        event: Event,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> (canvas::event::Status, Option<Message>) {
        let cursor_position = cursor.position_in(bounds);

        match event {
            Event::Mouse(mouse_event) => match mouse_event {
                mouse::Event::ButtonPressed(mouse::Button::Left) => {
                    if let Some(pos) = cursor_position {
                        if let Some(idx) = self.machine_at_position(pos, &bounds) {
                            // Don't allow dragging self
                            if !self.machines[idx].is_self {
                                return (
                                    canvas::event::Status::Captured,
                                    Some(Message::CanvasEvent(CanvasEvent::MachinePressed(idx))),
                                );
                            }
                        }
                    }
                }
                mouse::Event::ButtonReleased(mouse::Button::Left) => {
                    if self.drag.is_dragging() {
                        return (
                            canvas::event::Status::Captured,
                            Some(Message::CanvasEvent(CanvasEvent::MouseReleased)),
                        );
                    }
                }
                mouse::Event::CursorMoved { .. } => {
                    if self.drag.is_dragging() {
                        if let Some(pos) = cursor_position {
                            // Calculate snap target
                            let snap_target = self.find_snap_target(pos, &bounds);
                            return (
                                canvas::event::Status::Captured,
                                Some(Message::CanvasEvent(CanvasEvent::MouseMoved(pos, snap_target))),
                            );
                        }
                    }
                }
                _ => {}
            },
            _ => {}
        }

        (canvas::event::Status::Ignored, None)
    }

    fn draw(
        &self,
        _state: &Self::State,
        renderer: &Renderer,
        _theme: &Theme,
        bounds: Rectangle,
        _cursor: mouse::Cursor,
    ) -> Vec<Geometry> {
        let geometry = self.cache.draw(renderer, bounds.size(), |frame| {
            // Background
            frame.fill_rectangle(
                Point::ORIGIN,
                frame.size(),
                theme::BACKGROUND,
            );

            // Draw connection dots between self and each neighbor
            let self_center = self.grid_to_canvas(GridPos::origin(), &bounds);
            for machine in &self.machines {
                if !machine.is_self {
                    let neighbor_center = self.grid_to_canvas(machine.grid_pos, &bounds);
                    // Calculate midpoint between self and neighbor
                    let mid = Point::new(
                        (self_center.x + neighbor_center.x) / 2.0,
                        (self_center.y + neighbor_center.y) / 2.0,
                    );
                    // Draw connection dot
                    let dot = Path::circle(mid, theme::CONNECTION_DOT_RADIUS);
                    frame.fill(&dot, theme::CONNECTION_DOT);
                }
            }

            // Draw available snap positions
            for &pos in &self.available_positions {
                let target_center = self.grid_to_canvas(pos, &bounds);
                let highlighted = self.drag.snap_target == Some(pos);
                self.draw_snap_target(frame, target_center, highlighted);
            }

            // Draw machines (except the one being dragged)
            for (idx, machine) in self.machines.iter().enumerate() {
                let is_being_dragged = self.drag.dragging == Some(idx);
                if !is_being_dragged {
                    let center = self.grid_to_canvas(machine.grid_pos, &bounds);
                    self.draw_machine(frame, machine, center, false);
                }
            }

            // Draw dragged machine at cursor position
            if let Some(drag_idx) = self.drag.dragging {
                if let Some(cursor_pos) = self.drag.cursor_pos {
                    let machine = &self.machines[drag_idx];
                    self.draw_machine(frame, machine, cursor_pos, true);
                }
            }
        });

        vec![geometry]
    }

    fn mouse_interaction(
        &self,
        _state: &Self::State,
        bounds: Rectangle,
        cursor: mouse::Cursor,
    ) -> mouse::Interaction {
        if self.drag.is_dragging() {
            return mouse::Interaction::Grabbing;
        }

        if let Some(pos) = cursor.position_in(bounds) {
            if let Some(idx) = self.machine_at_position(pos, &bounds) {
                if !self.machines[idx].is_self {
                    return mouse::Interaction::Grab;
                }
            }
        }

        mouse::Interaction::default()
    }
}

/// Create the canvas widget
pub fn machine_canvas(
    machines: Vec<MachineNode>,
    drag: DragState,
    available_positions: Vec<GridPos>,
) -> Canvas<MachineGrid, Message> {
    Canvas::new(MachineGrid::new(machines, drag, available_positions))
        .width(iced::Length::Fill)
        .height(iced::Length::Fill)
}
