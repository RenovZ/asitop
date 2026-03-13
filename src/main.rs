use std::collections::VecDeque;
use std::fs;
use std::io::{self, Cursor};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use clap::{ArgAction, Parser};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use plist::{Dictionary, Value};
use ratatui::backend::CrosstermBackend;
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::prelude::{Color, Modifier, Style};
use ratatui::widgets::{Block, Borders};
use ratatui::{Frame, Terminal};
use sysinfo::System;
use tempfile::NamedTempFile;

#[derive(Debug, Parser, Clone)]
#[command(
    name = "asitop",
    version,
    about = "asitop: Performance monitoring CLI tool for Apple Silicon"
)]
struct Args {
    #[arg(
        long,
        default_value_t = 1,
        help = "Display interval and sampling interval for powermetrics (seconds)"
    )]
    interval: u64,
    #[arg(long, default_value_t = 2, help = "Choose display color (0~8)")]
    color: u8,
    #[arg(
        long,
        default_value_t = 30,
        help = "Interval for averaged values (seconds)"
    )]
    avg: u64,
    #[arg(
        long = "show_cores",
        alias = "show-cores",
        action = ArgAction::Set,
        default_value_t = false,
        help = "Choose show cores mode"
    )]
    show_cores: bool,
    #[arg(
        long = "max_count",
        alias = "max-count",
        default_value_t = 0,
        help = "Max show count to restart powermetrics"
    )]
    max_count: u64,
}

#[derive(Debug, Clone)]
struct CoreMetric {
    cpu: usize,
    active_percent: u16,
    freq_mhz: u64,
}

#[derive(Debug, Clone)]
struct Snapshot {
    timestamp: String,
    thermal_pressure: String,
    e_cluster_active: u16,
    e_cluster_freq_mhz: u64,
    p_cluster_active: u16,
    p_cluster_freq_mhz: u64,
    e_cores: Vec<CoreMetric>,
    p_cores: Vec<CoreMetric>,
    gpu_active: u16,
    gpu_freq_mhz: u64,
    ane_watts: f64,
    cpu_watts: f64,
    gpu_watts: f64,
    package_watts: f64,
}

#[derive(Debug, Clone)]
struct MemorySnapshot {
    total_gb: f64,
    used_gb: f64,
    swap_total_gb: f64,
    swap_used_gb: f64,
    used_percent: u16,
}

#[derive(Debug, Clone)]
struct SocInfo {
    name: String,
    e_core_count: usize,
    p_core_count: usize,
    gpu_core_count: usize,
    cpu_max_power: f64,
    gpu_max_power: f64,
}

#[derive(Debug)]
struct App {
    args: Args,
    soc: SocInfo,
    system: System,
    last_snapshot: Option<Snapshot>,
    memory: MemorySnapshot,
    cpu_history: VecDeque<u64>,
    gpu_history: VecDeque<u64>,
    package_samples: VecDeque<f64>,
    cpu_samples: VecDeque<f64>,
    gpu_samples: VecDeque<f64>,
    package_peak: f64,
    cpu_peak: f64,
    gpu_peak: f64,
}

impl App {
    fn new(args: Args, soc: SocInfo) -> Self {
        Self {
            args,
            soc,
            system: System::new_all(),
            last_snapshot: None,
            memory: MemorySnapshot {
                total_gb: 0.0,
                used_gb: 0.0,
                swap_total_gb: 0.0,
                swap_used_gb: 0.0,
                used_percent: 0,
            },
            cpu_history: VecDeque::with_capacity(120),
            gpu_history: VecDeque::with_capacity(120),
            package_samples: VecDeque::with_capacity(120),
            cpu_samples: VecDeque::with_capacity(120),
            gpu_samples: VecDeque::with_capacity(120),
            package_peak: 0.0,
            cpu_peak: 0.0,
            gpu_peak: 0.0,
        }
    }

    fn apply_snapshot(&mut self, snapshot: Snapshot) {
        self.memory = read_memory_snapshot(&mut self.system);
        let max_len = usize::max(1, (self.args.avg / self.args.interval.max(1)) as usize);
        push_history(
            &mut self.cpu_history,
            snapshot.cpu_watts_to_percent(self.soc.cpu_max_power),
        );
        push_history(
            &mut self.gpu_history,
            snapshot.gpu_watts_to_percent(self.soc.gpu_max_power),
        );
        push_sample(&mut self.package_samples, snapshot.package_watts, max_len);
        push_sample(&mut self.cpu_samples, snapshot.cpu_watts, max_len);
        push_sample(&mut self.gpu_samples, snapshot.gpu_watts, max_len);
        self.package_peak = self.package_peak.max(snapshot.package_watts);
        self.cpu_peak = self.cpu_peak.max(snapshot.cpu_watts);
        self.gpu_peak = self.gpu_peak.max(snapshot.gpu_watts);
        self.last_snapshot = Some(snapshot);
    }
}

impl Snapshot {
    fn cpu_watts_to_percent(&self, max_power: f64) -> u64 {
        power_percent(self.cpu_watts, max_power) as u64
    }

    fn gpu_watts_to_percent(&self, max_power: f64) -> u64 {
        power_percent(self.gpu_watts, max_power) as u64
    }
}

struct Monitor {
    output_file: NamedTempFile,
    child: Child,
}

impl Monitor {
    fn start(interval_secs: u64) -> Result<Self> {
        let output_file = NamedTempFile::new_in("/tmp").context("create powermetrics temp file")?;
        let child = spawn_powermetrics(output_file.path(), interval_secs)?;
        Ok(Self { output_file, child })
    }

    fn restart(&mut self, interval_secs: u64) -> Result<()> {
        self.stop();
        self.output_file =
            NamedTempFile::new_in("/tmp").context("create powermetrics temp file")?;
        self.child = spawn_powermetrics(self.output_file.path(), interval_secs)?;
        Ok(())
    }

    fn read_snapshot(&self) -> Result<Option<Snapshot>> {
        read_latest_plist(self.output_file.path())
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        self.stop();
    }
}

struct Tui {
    terminal: Terminal<CrosstermBackend<io::Stdout>>,
}

impl Tui {
    fn enter() -> Result<Self> {
        enable_raw_mode().context("enable raw mode")?;
        let mut stdout = io::stdout();
        execute!(stdout, EnterAlternateScreen).context("enter alternate screen")?;
        let backend = CrosstermBackend::new(stdout);
        let terminal = Terminal::new(backend).context("create terminal")?;
        Ok(Self { terminal })
    }

    fn draw(&mut self, app: &App) -> Result<()> {
        self.terminal
            .draw(|frame| draw_ui(frame, app))
            .context("draw ui")?;
        Ok(())
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(self.terminal.backend_mut(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}

fn main() -> Result<()> {
    let args = Args::parse();
    let soc = read_soc_info().context("read SoC info")?;

    println!("\nASITOP - Performance monitoring CLI tool for Apple Silicon");
    println!("You can update ASITOP by running `pip install asitop --upgrade`");
    println!("Get help at `https://github.com/tlkh/asitop`");
    println!("P.S. You are recommended to run ASITOP with `sudo asitop`\n");
    println!("\n[1/3] Loading ASITOP\n");
    println!("\u{1b}[?25l");
    println!("\n[2/3] Starting powermetrics process\n");

    let mut monitor = Monitor::start(args.interval)?;

    println!("\n[3/3] Waiting for first reading...\n");
    let mut latest = wait_for_first_snapshot(&monitor)?;
    let mut app = App::new(args.clone(), soc);
    app.apply_snapshot(latest.clone());

    print!("\u{1b}[2J\u{1b}[H");

    let mut tui = Tui::enter()?;
    let mut last_refresh = Instant::now();
    let mut refresh_count = 0_u64;

    loop {
        tui.draw(&app)?;
        if event::poll(Duration::from_millis(100)).context("poll terminal event")? {
            if let Event::Key(key) = event::read().context("read terminal event")? {
                let ctrl_c =
                    key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
                if key.kind == KeyEventKind::Press
                    && (ctrl_c || matches!(key.code, KeyCode::Char('q') | KeyCode::Esc))
                {
                    break;
                }
            }
        }

        if last_refresh.elapsed() >= Duration::from_secs(args.interval.max(1)) {
            if args.max_count > 0 && refresh_count >= args.max_count {
                monitor.restart(args.interval)?;
                latest = wait_for_first_snapshot(&monitor)?;
                app.apply_snapshot(latest.clone());
                refresh_count = 0;
            } else if let Some(next) = monitor.read_snapshot()? {
                if next.timestamp > latest.timestamp {
                    latest = next;
                    app.apply_snapshot(latest.clone());
                    refresh_count += 1;
                }
            }
            last_refresh = Instant::now();
        }
    }

    println!("Stopping...");
    println!("\u{1b}[?25h");
    Ok(())
}

fn wait_for_first_snapshot(monitor: &Monitor) -> Result<Snapshot> {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(snapshot) = monitor.read_snapshot()? {
            return Ok(snapshot);
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for powermetrics output");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn spawn_powermetrics(output_path: &Path, interval_secs: u64) -> Result<Child> {
    Command::new("sudo")
        .args([
            "nice",
            "-n",
            "10",
            "powermetrics",
            "--samplers",
            "cpu_power,gpu_power,thermal",
            "-o",
            output_path
                .to_str()
                .ok_or_else(|| anyhow!("non-utf8 temp path"))?,
            "-f",
            "plist",
            "-i",
            &(interval_secs.max(1) * 1000).to_string(),
        ])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .context("spawn powermetrics via sudo")
}

fn read_latest_plist(path: &Path) -> Result<Option<Snapshot>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).context("read powermetrics output"),
    };
    if bytes.is_empty() {
        return Ok(None);
    }

    for chunk in bytes.split(|byte| *byte == 0).rev() {
        if chunk.is_empty() {
            continue;
        }
        if let Ok(value) = Value::from_reader(Cursor::new(chunk)) {
            return parse_snapshot(&value).map(Some);
        }
    }
    Ok(None)
}

fn parse_snapshot(root: &Value) -> Result<Snapshot> {
    let dict = root
        .as_dictionary()
        .ok_or_else(|| anyhow!("powermetrics plist root is not a dictionary"))?;
    let thermal_pressure =
        get_string(dict, "thermal_pressure").unwrap_or_else(|| "Unknown".to_string());
    let timestamp = get_timestamp(dict, "timestamp").unwrap_or_default();
    let processor = get_dict(dict, "processor")?;
    let gpu = get_dict(dict, "gpu")?;

    let mut e_cluster_active_values = Vec::new();
    let mut e_cluster_freq_values = Vec::new();
    let mut p_cluster_active_values = Vec::new();
    let mut p_cluster_freq_values = Vec::new();
    let mut e_cluster_active = None;
    let mut e_cluster_freq = None;
    let mut e_cores = Vec::new();
    let mut p_cores = Vec::new();

    for cluster in get_array(processor, "clusters")? {
        let cluster_dict = cluster
            .as_dictionary()
            .ok_or_else(|| anyhow!("cluster entry is not a dictionary"))?;
        let name = get_string(cluster_dict, "name").unwrap_or_default();
        let cluster_freq = hz_to_mhz(get_f64(cluster_dict, "freq_hz").unwrap_or_default());
        let cluster_active =
            idle_ratio_to_percent(get_f64(cluster_dict, "idle_ratio").unwrap_or_default());

        if name.starts_with('E') {
            if name == "E-Cluster" {
                e_cluster_active = Some(cluster_active);
                e_cluster_freq = Some(cluster_freq);
            }
            e_cluster_active_values.push(cluster_active);
            e_cluster_freq_values.push(cluster_freq);
        } else if name.starts_with('P') {
            p_cluster_active_values.push(cluster_active);
            p_cluster_freq_values.push(cluster_freq);
        }

        for cpu in get_array(cluster_dict, "cpus")? {
            let cpu_dict = cpu
                .as_dictionary()
                .ok_or_else(|| anyhow!("cpu entry is not a dictionary"))?;
            let core = CoreMetric {
                cpu: get_usize(cpu_dict, "cpu").unwrap_or_default(),
                active_percent: idle_ratio_to_percent(
                    get_f64(cpu_dict, "idle_ratio").unwrap_or_default(),
                ),
                freq_mhz: hz_to_mhz(get_f64(cpu_dict, "freq_hz").unwrap_or_default()),
            };
            if name.starts_with('E') {
                e_cores.push(core);
            } else {
                p_cores.push(core);
            }
        }
    }

    Ok(Snapshot {
        timestamp,
        thermal_pressure,
        e_cluster_active: e_cluster_active
            .unwrap_or_else(|| average_percent(&e_cluster_active_values)),
        e_cluster_freq_mhz: e_cluster_freq.unwrap_or_else(|| max_u64(&e_cluster_freq_values)),
        p_cluster_active: average_percent(&p_cluster_active_values),
        p_cluster_freq_mhz: max_u64(&p_cluster_freq_values),
        e_cores,
        p_cores,
        gpu_active: idle_ratio_to_percent(get_f64(gpu, "idle_ratio").unwrap_or_default()),
        gpu_freq_mhz: get_f64(gpu, "freq_hz").unwrap_or_default().round() as u64,
        ane_watts: mj_to_watts(get_f64(processor, "ane_energy").unwrap_or_default()),
        cpu_watts: mj_to_watts(get_f64(processor, "cpu_energy").unwrap_or_default()),
        gpu_watts: mj_to_watts(get_f64(processor, "gpu_energy").unwrap_or_default()),
        package_watts: mj_to_watts(get_f64(processor, "combined_power").unwrap_or_default()),
    })
}

fn draw_ui(frame: &mut Frame, app: &App) {
    let Some(snapshot) = &app.last_snapshot else {
        return;
    };
    let color = color_from_index(app.args.color);
    let cpu_title = format!(
        "{} (cores: {}E+{}P+{}GPU)",
        app.soc.name, app.soc.e_core_count, app.soc.p_core_count, app.soc.gpu_core_count
    );

    if app.args.show_cores {
        let root = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
            .split(frame.area());

        let left = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(22), Constraint::Min(8)])
            .split(root[0]);
        render_processor_panel(frame, left[0], &cpu_title, snapshot, color, true);
        render_power_panel(frame, left[1], app, snapshot);

        let right = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(5), Constraint::Min(8)])
            .split(root[1]);
        render_memory_panel(frame, right[0], app, color);
        render_cores_panel(frame, right[1], snapshot, color);
    } else {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(34),
                Constraint::Percentage(22),
                Constraint::Percentage(44),
            ])
            .split(frame.area());
        render_processor_panel(frame, rows[0], &cpu_title, snapshot, color, false);
        render_memory_panel(frame, rows[1], app, color);
        render_power_panel(frame, rows[2], app, snapshot);
    }
}

fn render_processor_panel(
    frame: &mut Frame,
    area: Rect,
    title: &str,
    snapshot: &Snapshot,
    color: Color,
    show_cores: bool,
) {
    frame.render_widget(panel_block(title, color), area);
    if show_cores {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .margin(1)
            .constraints([
                Constraint::Length(3),
                Constraint::Length(5),
                Constraint::Length(3),
                Constraint::Length(8),
                Constraint::Length(3),
                Constraint::Length(3),
            ])
            .split(area);
        frame.render_widget(
            Block::default().title("E-CPU Usage").borders(Borders::ALL),
            rows[0],
        );
        draw_segmented_meter(
            frame.buffer_mut(),
            inner_area(rows[0]),
            snapshot.e_cluster_active,
            &format!(
                "{}% @ {} MHz",
                snapshot.e_cluster_active, snapshot.e_cluster_freq_mhz
            ),
            color,
        );
        render_core_grid(frame, rows[1], &snapshot.e_cores, color, true);
        frame.render_widget(
            Block::default().title("P-CPU Usage").borders(Borders::ALL),
            rows[2],
        );
        draw_segmented_meter(
            frame.buffer_mut(),
            inner_area(rows[2]),
            snapshot.p_cluster_active,
            &format!(
                "{}% @ {} MHz",
                snapshot.p_cluster_active, snapshot.p_cluster_freq_mhz
            ),
            color,
        );
        render_core_grid(frame, rows[3], &snapshot.p_cores, color, false);
        let bottom = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
            .split(rows[4]);
        frame.render_widget(
            Block::default().title("GPU Usage").borders(Borders::ALL),
            bottom[0],
        );
        draw_segmented_meter(
            frame.buffer_mut(),
            inner_area(bottom[0]),
            snapshot.gpu_active,
            &format!("{}% @ {} MHz", snapshot.gpu_active, snapshot.gpu_freq_mhz),
            color,
        );
        let ane_percent = ((snapshot.ane_watts / 8.0) * 100.0).clamp(0.0, 100.0) as u16;
        frame.render_widget(
            Block::default().title("ANE").borders(Borders::ALL),
            bottom[1],
        );
        draw_segmented_meter(
            frame.buffer_mut(),
            inner_area(bottom[1]),
            ane_percent,
            &format!("{}% @ {:.1} W", ane_percent, snapshot.ane_watts),
            color,
        );
    } else {
        let content = inner_area(area);
        let [top_row, bottom_row] = split_vertical_with_gap(content, 1);
        let [top_left, top_right] = split_horizontal_with_gap(top_row, 1);
        draw_segmented_meter(
            frame.buffer_mut(),
            top_left,
            snapshot.e_cluster_active,
            &format!(
                "E-CPU Usage: {}% @ {} MHz",
                snapshot.e_cluster_active, snapshot.e_cluster_freq_mhz
            ),
            color,
        );
        draw_segmented_meter(
            frame.buffer_mut(),
            top_right,
            snapshot.p_cluster_active,
            &format!(
                "P-CPU Usage: {}% @ {} MHz",
                snapshot.p_cluster_active, snapshot.p_cluster_freq_mhz
            ),
            color,
        );
        let [bottom_left, bottom_right] = split_horizontal_with_gap(bottom_row, 1);
        draw_segmented_meter(
            frame.buffer_mut(),
            bottom_left,
            snapshot.gpu_active,
            &format!(
                "GPU Usage: {}% @ {} MHz",
                snapshot.gpu_active, snapshot.gpu_freq_mhz
            ),
            color,
        );
        let ane_percent = ((snapshot.ane_watts / 8.0) * 100.0).clamp(0.0, 100.0) as u16;
        draw_segmented_meter(
            frame.buffer_mut(),
            bottom_right,
            ane_percent,
            &format!("ANE Usage: {}% @ {:.1} W", ane_percent, snapshot.ane_watts),
            color,
        );
    }
}

fn render_memory_panel(frame: &mut Frame, area: Rect, app: &App, color: Color) {
    let label = if app.memory.swap_total_gb < 0.1 {
        format!(
            "RAM Usage: {:.1}/{:.1}GB - swap inactive",
            app.memory.used_gb, app.memory.total_gb
        )
    } else {
        format!(
            "RAM Usage: {:.1}/{:.1}GB - swap:{:.1}/{:.1}GB",
            app.memory.used_gb,
            app.memory.total_gb,
            app.memory.swap_used_gb,
            app.memory.swap_total_gb
        )
    };
    frame.render_widget(panel_block("Memory", color), area);
    let content = inner_area(area);
    draw_segmented_meter(
        frame.buffer_mut(),
        content,
        app.memory.used_percent.min(100),
        &label,
        color,
    );
}

fn render_power_panel(frame: &mut Frame, area: Rect, app: &App, snapshot: &Snapshot) {
    let color = color_from_index(app.args.color);
    let title = format!(
        "CPU+GPU+ANE Power: {:.2}W (avg: {:.2}W peak: {:.2}W) throttle: {}",
        snapshot.package_watts,
        average(&app.package_samples),
        app.package_peak,
        if snapshot.thermal_pressure == "Nominal" {
            "no"
        } else {
            "yes"
        }
    );
    frame.render_widget(panel_block(&title, color), area);
    let content = inner_area(area);
    let [left, right] = split_horizontal_with_gap(content, 1);
    draw_history_plot(
        frame.buffer_mut(),
        left,
        &format!(
            "CPU: {:.2}W (avg: {:.2}W peak: {:.2}W)",
            snapshot.cpu_watts,
            average(&app.cpu_samples),
            app.cpu_peak
        ),
        &app.cpu_history,
        color,
    );
    draw_history_plot(
        frame.buffer_mut(),
        right,
        &format!(
            "GPU: {:.2}W (avg: {:.2}W peak: {:.2}W)",
            snapshot.gpu_watts,
            average(&app.gpu_samples),
            app.gpu_peak
        ),
        &app.gpu_history,
        color,
    );
}

fn render_cores_panel(frame: &mut Frame, area: Rect, snapshot: &Snapshot, color: Color) {
    frame.render_widget(Block::default().title("Cores").borders(Borders::ALL), area);
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .margin(1)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);
    render_core_grid(frame, cols[0], &snapshot.e_cores, color, true);
    render_core_grid(frame, cols[1], &snapshot.p_cores, color, false);
}

fn render_core_grid(
    frame: &mut Frame,
    area: Rect,
    cores: &[CoreMetric],
    color: Color,
    e_cluster: bool,
) {
    if cores.is_empty() {
        return;
    }
    let constraints: Vec<Constraint> = cores.iter().map(|_| Constraint::Length(3)).collect();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);
    for (idx, core) in cores.iter().enumerate() {
        let name = if e_cluster || cores.len() < 6 {
            format!("Core-{} ", core.cpu + 1)
        } else {
            format!("C-{} ", core.cpu + 1)
        };
        frame.render_widget(Block::default().borders(Borders::ALL), rows[idx]);
        draw_segmented_meter(
            frame.buffer_mut(),
            inner_area(rows[idx]),
            core.active_percent,
            &format!("{name}{}% @ {} MHz", core.active_percent, core.freq_mhz),
            color,
        );
    }
}

fn inner_area(area: Rect) -> Rect {
    Rect {
        x: area.x.saturating_add(1),
        y: area.y.saturating_add(1),
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    }
}

fn draw_segmented_meter(buf: &mut Buffer, area: Rect, percent: u16, label: &str, color: Color) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let label_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
    let on = Style::default().bg(color);
    let off = Style::default().fg(color);
    buf.set_stringn(area.x, area.y, label, area.width as usize, label_style);
    let plot_area = Rect {
        x: area.x,
        y: area.y.saturating_add(1),
        width: area.width,
        height: area.height.saturating_sub(1),
    };
    if plot_area.width == 0 || plot_area.height == 0 {
        return;
    }
    let filled = ((plot_area.width as f32) * (percent.min(100) as f32 / 100.0)).round() as u16;
    for x in 0..plot_area.width {
        for y in 0..plot_area.height {
            if x < filled {
                if x % 2 == 0 {
                    buf[(plot_area.x + x, plot_area.y + y)]
                        .set_symbol(" ")
                        .set_style(on);
                } else {
                    buf[(plot_area.x + x, plot_area.y + y)]
                        .set_symbol("▏")
                        .set_style(off);
                }
            } else {
                buf[(plot_area.x + x, plot_area.y + y)]
                    .set_symbol("▏")
                    .set_style(off);
            }
        }
    }
    // Keep a visible moving edge so growth/shrink has a clear dynamic boundary.
    if filled < plot_area.width {
        for y in 0..plot_area.height {
            buf[(plot_area.x + filled, plot_area.y + y)]
                .set_symbol("▏")
                .set_style(off);
        }
    }
}

fn draw_history_plot(
    buf: &mut Buffer,
    area: Rect,
    label: &str,
    data: &VecDeque<u64>,
    color: Color,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let values: Vec<u64> = data.iter().copied().collect();
    let count = values.len().max(1);
    let max_value = values.iter().copied().max().unwrap_or(1).max(1);
    let label_style = Style::default().fg(color).add_modifier(Modifier::BOLD);
    let fill_style = Style::default().bg(color);
    let empty_style = Style::default();
    buf.set_stringn(area.x, area.y, label, area.width as usize, label_style);
    let plot_area = Rect {
        x: area.x,
        y: area.y.saturating_add(1),
        width: area.width,
        height: area.height.saturating_sub(1),
    };
    for x in 0..plot_area.width {
        let idx =
            ((x as usize) * count / plot_area.width.max(1) as usize).min(count.saturating_sub(1));
        let value = values.get(idx).copied().unwrap_or(0);
        let height = ((value as f32 / max_value as f32) * plot_area.height as f32).round() as u16;
        for y in 0..plot_area.height {
            if y >= plot_area.height.saturating_sub(height) {
                buf[(plot_area.x + x, plot_area.y + y)]
                    .set_symbol(" ")
                    .set_style(fill_style);
            } else {
                buf[(plot_area.x + x, plot_area.y + y)]
                    .set_symbol(" ")
                    .set_style(empty_style);
            }
        }
    }
}

fn split_horizontal_with_gap(area: Rect, gap: u16) -> [Rect; 2] {
    let gap = gap.min(area.width);
    let left_width = area.width.saturating_sub(gap) / 2;
    let right_x = area.x + left_width + gap;
    let right_width = area.width.saturating_sub(left_width + gap);
    [
        Rect {
            x: area.x,
            y: area.y,
            width: left_width,
            height: area.height,
        },
        Rect {
            x: right_x,
            y: area.y,
            width: right_width,
            height: area.height,
        },
    ]
}

fn split_vertical_with_gap(area: Rect, gap: u16) -> [Rect; 2] {
    let gap = gap.min(area.height);
    let top_height = area.height.saturating_sub(gap) / 2;
    let bottom_y = area.y + top_height + gap;
    let bottom_height = area.height.saturating_sub(top_height + gap);
    [
        Rect {
            x: area.x,
            y: area.y,
            width: area.width,
            height: top_height,
        },
        Rect {
            x: area.x,
            y: bottom_y,
            width: area.width,
            height: bottom_height,
        },
    ]
}

fn panel_block<'a>(title: &'a str, color: Color) -> Block<'a> {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_style(Style::default().fg(color))
        .title_style(Style::default().fg(color).add_modifier(Modifier::BOLD))
}

fn read_memory_snapshot(system: &mut System) -> MemorySnapshot {
    system.refresh_memory();
    let total_gb = bytes_to_gb(system.total_memory());
    let used_gb = bytes_to_gb(system.used_memory());
    let swap_total_gb = bytes_to_gb(system.total_swap());
    let swap_used_gb = bytes_to_gb(system.used_swap());
    let used_percent = if total_gb <= f64::EPSILON {
        0
    } else {
        ((used_gb / total_gb) * 100.0).round().clamp(0.0, 100.0) as u16
    };
    MemorySnapshot {
        total_gb,
        used_gb,
        swap_total_gb,
        swap_used_gb,
        used_percent,
    }
}

fn read_soc_info() -> Result<SocInfo> {
    let name = run_command("sysctl", &["-n", "machdep.cpu.brand_string"])?
        .trim()
        .to_string();
    let e_core_count = run_command("sysctl", &["-n", "hw.perflevel1.logicalcpu"])
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    let p_core_count = run_command("sysctl", &["-n", "hw.perflevel0.logicalcpu"])
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0);
    let gpu_core_count = run_command(
        "system_profiler",
        &["-detailLevel", "basic", "SPDisplaysDataType"],
    )
    .ok()
    .and_then(|output| {
        output
            .lines()
            .find(|line| line.contains("Total Number of Cores"))
            .and_then(|line| line.rsplit(": ").next())
            .and_then(|value| value.trim().parse().ok())
    })
    .unwrap_or(0);
    let (cpu_max_power, gpu_max_power) = power_budget_for_soc(&name);
    Ok(SocInfo {
        name,
        e_core_count,
        p_core_count,
        gpu_core_count,
        cpu_max_power,
        gpu_max_power,
    })
}

fn run_command(command: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(command)
        .args(args)
        .output()
        .with_context(|| format!("run {} {:?}", command, args))?;
    if !output.status.success() {
        bail!("{} {:?} exited with {}", command, args, output.status);
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn power_budget_for_soc(name: &str) -> (f64, f64) {
    match name {
        "Apple M1 Max" => (30.0, 60.0),
        "Apple M1 Pro" => (30.0, 30.0),
        "Apple M1 Ultra" => (60.0, 120.0),
        "Apple M2" => (25.0, 15.0),
        _ => (20.0, 20.0),
    }
}

fn bytes_to_gb(value: u64) -> f64 {
    value as f64 / 1024.0 / 1024.0 / 1024.0
}

fn hz_to_mhz(value: f64) -> u64 {
    (value / 1_000_000.0).round() as u64
}

fn mj_to_watts(value: f64) -> f64 {
    value / 1000.0
}

fn idle_ratio_to_percent(value: f64) -> u16 {
    ((1.0 - value) * 100.0).round().clamp(0.0, 100.0) as u16
}

fn power_percent(value: f64, max_power: f64) -> u16 {
    if max_power <= f64::EPSILON {
        0
    } else {
        ((value / max_power) * 100.0).round().clamp(0.0, 100.0) as u16
    }
}

fn push_history(history: &mut VecDeque<u64>, value: u64) {
    if history.len() == 120 {
        history.pop_front();
    }
    history.push_back(value);
}

fn push_sample(history: &mut VecDeque<f64>, value: f64, max_len: usize) {
    if history.len() == max_len {
        history.pop_front();
    }
    history.push_back(value);
}

fn average(values: &VecDeque<f64>) -> f64 {
    if values.is_empty() {
        0.0
    } else {
        values.iter().sum::<f64>() / values.len() as f64
    }
}

fn average_percent(values: &[u16]) -> u16 {
    if values.is_empty() {
        0
    } else {
        (values.iter().map(|value| *value as u64).sum::<u64>() / values.len() as u64) as u16
    }
}

fn max_u64(values: &[u64]) -> u64 {
    values.iter().copied().max().unwrap_or_default()
}

fn get_array<'a>(dict: &'a Dictionary, key: &str) -> Result<&'a Vec<Value>> {
    dict.get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("missing array key {}", key))
}

fn get_dict<'a>(dict: &'a Dictionary, key: &str) -> Result<&'a Dictionary> {
    dict.get(key)
        .and_then(Value::as_dictionary)
        .ok_or_else(|| anyhow!("missing dictionary key {}", key))
}

fn get_string(dict: &Dictionary, key: &str) -> Option<String> {
    dict.get(key)
        .and_then(Value::as_string)
        .map(ToOwned::to_owned)
}

fn get_f64(dict: &Dictionary, key: &str) -> Option<f64> {
    dict.get(key).and_then(value_as_f64)
}

fn get_timestamp(dict: &Dictionary, key: &str) -> Option<String> {
    dict.get(key).and_then(value_as_timestamp)
}

fn get_usize(dict: &Dictionary, key: &str) -> Option<usize> {
    dict.get(key)
        .and_then(value_as_u64)
        .map(|value| value as usize)
}

fn value_as_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Integer(integer) => integer
            .as_unsigned()
            .or_else(|| integer.as_signed().map(|value| value as u64)),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

fn value_as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Integer(integer) => integer
            .as_signed()
            .map(|value| value as f64)
            .or_else(|| integer.as_unsigned().map(|value| value as f64)),
        Value::Real(value) => Some(*value),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

fn value_as_timestamp(value: &Value) -> Option<String> {
    match value {
        Value::Date(date) => Some(date.to_xml_format()),
        Value::String(text) => Some(text.clone()),
        Value::Integer(integer) => integer
            .as_signed()
            .map(|value| value.to_string())
            .or_else(|| integer.as_unsigned().map(|value| value.to_string())),
        Value::Real(value) => Some(value.to_string()),
        _ => None,
    }
}

fn color_from_index(index: u8) -> Color {
    match index {
        0 => Color::White,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        _ => Color::Green,
    }
}
