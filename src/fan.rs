// Copyright 2018-2021 System76 <info@system76.com>
//
// SPDX-License-Identifier: GPL-3.0-only

#![allow(clippy::inconsistent_digit_grouping)]

use std::{
    cell::Cell,
    cmp,
    collections::VecDeque,
    fs, io,
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use sysfs_class::{HwMon, SysClass};

#[derive(Debug, thiserror::Error)]
pub enum FanDaemonError {
    #[error("failed to collect hwmon devices: {}", _0)]
    HwmonDevices(io::Error),
    #[error("platform hwmon not found")]
    PlatformHwmonNotFound,
    #[error("cpu hwmon not found")]
    CpuHwmonNotFound,
}

pub struct FanDaemon {
    curve:             FanCurve,
    amdgpus:           Vec<HwMon>,
    platforms:         Vec<HwMon>,
    cpus:              Vec<HwMon>,
    nvidia_exists:     bool,
    displayed_warning: Cell<bool>,
    /// Smoothed, rate-limited controller. Only used on boards that opt in to it
    /// (currently Thelio Mira); other boards apply their curve directly.
    controller:        Option<FanController>,
    last_cpu_stat:     Option<(u64, u64)>,
}

impl FanDaemon {
    pub fn new(nvidia_exists: bool) -> Self {
        let model = fs::read_to_string("/sys/class/dmi/id/product_version").unwrap_or_default();
        let mut daemon = Self {
            curve: match model.trim() {
                "thelio-major-r1" => FanCurve::threadripper2(),
                "thelio-astra-a1" | "thelio-astra-a1.1" | "thelio-major-r2"
                | "thelio-major-r2.1" | "thelio-major-b1" | "thelio-major-b2"
                | "thelio-major-b3" | "thelio-mega-r1" | "thelio-mega-r1.1" => FanCurve::hedt(),
                "thelio-massive-b1" => FanCurve::xeon(),
                m if m.starts_with("thelio-mira") => FanCurve::pbo(),
                _ => FanCurve::standard(),
            },
            amdgpus: Vec::new(),
            platforms: Vec::new(),
            cpus: Vec::new(),
            nvidia_exists,
            displayed_warning: Cell::new(false),
            controller: None,
            last_cpu_stat: None,
        };
        if model.trim().starts_with("thelio-mira") {
            daemon.controller = Some(FanController::new(FanCurve::pbo(), FanConfig::default()));
        }

        if let Err(err) = daemon.discover() {
            log::error!("fan daemon: {}", err);
        }

        daemon
    }

    /// Discover all utilizable hwmon devices
    fn discover(&mut self) -> Result<(), FanDaemonError> {
        self.amdgpus.clear();
        self.platforms.clear();
        self.cpus.clear();

        for hwmon in HwMon::all().map_err(FanDaemonError::HwmonDevices)? {
            if let Ok(name) = hwmon.name() {
                log::debug!("hwmon: {}", name);

                match name.as_str() {
                    "amdgpu" => self.amdgpus.push(hwmon),
                    "system76" => (), // TODO: Support laptops
                    "system76_io" | "system76_thelio_io" => self.platforms.push(hwmon),
                    "apm_xgene" | "coretemp" | "k10temp" => self.cpus.push(hwmon),
                    _ => (),
                }
            }
        }

        if self.platforms.is_empty() {
            return Err(FanDaemonError::PlatformHwmonNotFound);
        }

        if self.cpus.is_empty() {
            return Err(FanDaemonError::CpuHwmonNotFound);
        }

        Ok(())
    }

    /// Get the maximum measured CPU and GPU temperatures, in thousandths of a Celsius.
    /// Thousandths celsius is the standard Linux hwmon temperature unit.
    fn get_temps(&self) -> (Option<u32>, Option<u32>) {
        let max_temp = |sensors: &[HwMon]| {
            sensors
                .iter()
                .filter_map(|sensor| sensor.temp(1).ok())
                .filter_map(|temp| temp.input().ok())
                .map(|input| input as u32)
                .max()
        };

        let cpu = max_temp(&self.cpus);
        let mut gpu = max_temp(&self.amdgpus);

        // Fetch NVIDIA temperatures from the `nvidia-smi` tool when it exists.
        if self.nvidia_exists && !self.displayed_warning.get() {
            let mut nv_temp = 0;
            match nvidia_temperatures(|temp| nv_temp = cmp::max(temp, nv_temp)) {
                Ok(()) => {
                    if nv_temp != 0 {
                        log::debug!("highest nvidia temp: {}", nv_temp);
                        gpu = Some(gpu.map_or(nv_temp * 1000, |temp| cmp::max(nv_temp * 1000, temp)));
                    }
                }
                Err(why) => {
                    log::warn!("failed to get temperature of NVIDIA GPUs: {}", why);
                    self.displayed_warning.set(true);
                }
            }
        }

        log::debug!("current cpu temp: {:?}, gpu temp: {:?}", cpu, gpu);

        (cpu, gpu)
    }

    /// Get the maximum measured temperature from any CPU / GPU on the system, in
    /// thousandths of a Celsius.
    pub fn get_temp(&self) -> Option<u32> {
        let (cpu, gpu) = self.get_temps();
        match (cpu, gpu) {
            (Some(c), Some(g)) => Some(cmp::max(c, g)),
            (c, g) => c.or(g),
        }
    }

    /// Overall CPU utilization since the previous call, 0.0 - 100.0.
    fn cpu_load(&mut self) -> f32 {
        let stat = fs::read_to_string("/proc/stat").unwrap_or_default();
        let fields: Vec<u64> = stat
            .lines()
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .skip(1)
            .filter_map(|f| f.parse().ok())
            .collect();
        if fields.len() < 5 {
            return 0.0;
        }
        let total: u64 = fields.iter().take(8).sum();
        let idle = fields[3] + fields[4];
        let load = match self.last_cpu_stat {
            Some((last_total, last_idle)) if total > last_total => {
                let dt = (total - last_total) as f32;
                let di = idle.saturating_sub(last_idle) as f32;
                100.0 * (dt - di).max(0.0) / dt
            }
            _ => 0.0,
        };
        self.last_cpu_stat = Some((total, idle));
        load
    }

    /// Get the correct duty cycle for a temperature in thousandths Celsius, from 0 to 255
    /// Thousandths celsius is the standard Linux hwmon temperature unit
    /// 0 to 255 is the standard Linux hwmon pwm unit
    pub fn get_duty(&self, temp: u32) -> Option<u8> {
        self.curve
            .get_duty((temp / 10) as i16)
            .map(|duty| (((u32::from(duty)) * 255) / 10_000) as u8)
    }

    /// Set the current duty cycle, from 0 to 255
    /// 0 to 255 is the standard Linux hwmon pwm unit
    pub fn set_duty(&self, duty_opt: Option<u8>) {
        if let Some(duty) = duty_opt {
            let duty_str = format!("{}", duty);
            for platform in &self.platforms {
                let _ = platform.write_file("pwm1_enable", "1");
                let _ = platform.write_file("pwm1", &duty_str);
                let _ = platform.write_file("pwm2", &duty_str);
                let _ = platform.write_file("pwm3", &duty_str);
                let _ = platform.write_file("pwm4", &duty_str);
            }
        } else {
            for platform in &self.platforms {
                let _ = platform.write_file("pwm1_enable", "2");
            }
        }
    }

    /// Calculate the correct duty cycle and apply it to all fans
    pub fn step(&mut self) {
        if self.discover().is_err() {
            return;
        }

        if self.controller.is_some() {
            let (cpu, gpu) = self.get_temps();
            let load = self.cpu_load();
            let duty = cpu.or(gpu).map(|cpu| {
                let temp = cmp::max(cpu, gpu.unwrap_or(0));
                let sample = Sample {
                    temp:     (temp / 10) as i16,
                    gpu_temp: gpu.map_or(0, |g| (g / 10) as i16),
                    cpu_load: load,
                };
                let hundredths = self
                    .controller
                    .as_mut()
                    .expect("checked above")
                    .update(sample, Instant::now());
                ((u32::from(hundredths) * 255) / 10_000) as u8
            });
            self.set_duty(duty);
        } else {
            self.set_duty(self.get_temp().and_then(|temp| self.get_duty(temp)));
        }
    }
}

impl Drop for FanDaemon {
    fn drop(&mut self) { self.set_duty(None); }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FanPoint {
    // Temperature in hundredths of a degree, 10000 = 100C
    temp: i16,
    // duty in hundredths of a percent, 10000 = 100%
    duty: u16,
}

impl FanPoint {
    pub const fn new(temp: i16, duty: u16) -> Self { Self { temp, duty } }

    /// Find the duty between two points and a given temperature, if the temperature
    /// lies within this range.
    fn get_duty_between_points(self, next: Self, temp: i16) -> Option<u16> {
        // If the temp matches the next point, return the next point duty
        if temp == next.temp {
            return Some(next.duty);
        }

        // If the temp matches the previous point, return the previous point duty
        if temp == self.temp {
            return Some(self.duty);
        }

        // If the temp is in between the previous and next points, interpolate the duty
        if self.temp < temp && next.temp > temp {
            return Some(self.interpolate_duties(next, temp));
        }

        None
    }

    /// Interpolates the current duty with that of the given next point and temperature.
    fn interpolate_duties(self, next: Self, temp: i16) -> u16 {
        let dtemp = next.temp - self.temp;
        let dduty = next.duty - self.duty;

        let slope = f32::from(dduty) / f32::from(dtemp);

        let temp_offset = temp - self.temp;
        let duty_offset = (slope * f32::from(temp_offset)).round();

        self.duty + (duty_offset as u16)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FanCurve {
    points: Vec<FanPoint>,
}

impl FanCurve {
    /// Adds a point to the fan curve
    #[must_use]
    pub fn append(mut self, temp: i16, duty: u16) -> Self {
        self.points.push(FanPoint::new(temp, duty));
        self
    }

    /// The standard fan curve
    pub fn standard() -> Self {
        Self::default()
            .append(44_99, 0_00)
            .append(45_00, 30_00)
            .append(55_00, 35_00)
            .append(65_00, 40_00)
            .append(75_00, 50_00)
            .append(78_00, 60_00)
            .append(81_00, 70_00)
            .append(84_00, 80_00)
            .append(86_00, 90_00)
            .append(88_00, 100_00)
    }

    /// Quiet-biased curve for high-temperature-tolerant CPUs (e.g. Ryzen with PBO):
    /// fans off at 40°C, 40% at 60°C, 100% at 90°C. Meant to be used with
    /// [`FanController`], which smooths and rate-limits the result.
    #[must_use]
    pub fn pbo() -> Self {
        Self::default().append(40_00, 0_00).append(60_00, 40_00).append(90_00, 100_00)
    }

    /// Fan curve for threadripper 2
    pub fn threadripper2() -> Self {
        Self::default()
            .append(00_00, 30_00)
            .append(40_00, 40_00)
            .append(47_50, 50_00)
            .append(55_00, 65_00)
            .append(62_50, 85_00)
            .append(66_25, 100_00)
    }

    /// Fan curve for HEDT systems
    pub fn hedt() -> Self {
        Self::default()
            .append(00_00, 30_00)
            .append(50_00, 35_00)
            .append(60_00, 45_00)
            .append(70_00, 55_00)
            .append(74_00, 60_00)
            .append(76_00, 70_00)
            .append(78_00, 80_00)
            .append(81_00, 100_00)
    }

    /// Fan curve for xeon
    pub fn xeon() -> Self {
        Self::default()
            .append(00_00, 40_00)
            .append(50_00, 40_00)
            .append(55_00, 45_00)
            .append(60_00, 50_00)
            .append(65_00, 55_00)
            .append(70_00, 60_00)
            .append(72_00, 65_00)
            .append(74_00, 80_00)
            .append(76_00, 85_00)
            .append(77_00, 90_00)
            .append(78_00, 100_00)
    }

    pub fn get_duty(&self, temp: i16) -> Option<u16> {
        // If the temp is less than the first point, return the first point duty
        if let Some(first) = self.points.first() {
            if temp < first.temp {
                return Some(first.duty);
            }
        }

        // Use when we upgrade to 1.28.0
        // for &[prev, next] in self.points.windows(2) {

        for window in self.points.windows(2) {
            let prev = window[0];
            let next = window[1];
            if let Some(duty) = prev.get_duty_between_points(next, temp) {
                return Some(duty);
            }
        }

        // If the temp is greater than the last point, return the last point duty
        if let Some(last) = self.points.last() {
            if temp > last.temp {
                return Some(last.duty);
            }
        }

        // If there are no points, return None
        None
    }
}

/// Tunables for [`FanController`]. Temperatures are in hundredths of a degree,
/// duties in hundredths of a percent.
#[derive(Clone, Copy, Debug)]
pub struct FanConfig {
    /// Temperature at which the curve reaches full duty.
    pub target_temp:              i16,
    /// Duty below which fans are considered inaudible; rises up to this are immediate.
    pub silence_threshold:        u16,
    /// CPU utilization (0-100) that counts as sustained load when near `target_temp`.
    pub sustained_load_threshold: f32,
    /// GPU temperature that on its own counts as sustained airflow demand.
    pub gpu_temp_threshold:       i16,
    /// Instantaneous temperature above `target_temp` that forces 100% immediately.
    pub critical_offset:          i16,
    /// Number of samples (seconds) averaged for the control temperature.
    pub smoothing_window:         usize,
    /// Seconds a lower target must persist before the duty is reduced.
    pub ramp_down_delay:          Duration,
    /// Seconds a demand condition must hold before the duty may rise above `silence_threshold`.
    pub sustained_delay:          Duration,
    /// Smallest downward duty change that is acted on.
    pub min_decrease:             u16,
    /// Duty increase per update once above `silence_threshold`.
    pub ramp_step:                u16,
}

impl Default for FanConfig {
    fn default() -> Self {
        Self {
            target_temp:              90_00,
            silence_threshold:        40_00,
            sustained_load_threshold: 50.0,
            gpu_temp_threshold:       70_00,
            critical_offset:          3_00,
            smoothing_window:         5,
            ramp_down_delay:          Duration::from_secs(10),
            sustained_delay:          Duration::from_secs(10),
            min_decrease:             2_00,
            ramp_step:                1_00,
        }
    }
}

/// One set of sensor readings fed to [`FanController::update`].
#[derive(Clone, Copy, Debug)]
pub struct Sample {
    /// Highest CPU/GPU temperature, hundredths of a degree.
    pub temp:     i16,
    /// GPU temperature, hundredths of a degree (0 if none).
    pub gpu_temp: i16,
    /// CPU utilization, 0-100.
    pub cpu_load: f32,
}

/// Stateful controller layered on a [`FanCurve`], ported from the Thelio Io Windows
/// service: averaged temperature, immediate rise to a quiet floor, a one-point-per-
/// update ramp above it that is only allowed under sustained demand, delayed
/// ramp-down, and an instant full-speed override for critical temperatures.
#[derive(Clone, Debug)]
pub struct FanController {
    curve:                FanCurve,
    config:               FanConfig,
    history:              VecDeque<i16>,
    duty:                 Option<u16>,
    below_since:          Option<Instant>,
    demand_since:         Option<Instant>,
}

impl FanController {
    #[must_use]
    pub fn new(curve: FanCurve, config: FanConfig) -> Self {
        Self {
            curve,
            config,
            history: VecDeque::new(),
            duty: None,
            below_since: None,
            demand_since: None,
        }
    }

    /// Feed one sample and get the duty to apply, in hundredths of a percent.
    pub fn update(&mut self, sample: Sample, now: Instant) -> u16 {
        let cfg = self.config;

        self.history.push_back(sample.temp);
        while self.history.len() > cfg.smoothing_window.max(1) {
            self.history.pop_front();
        }
        let avg = (self.history.iter().map(|&t| i32::from(t)).sum::<i32>()
            / self.history.len() as i32) as i16;

        // Critical override ignores averaging, delays and ramp limits.
        if sample.temp >= cfg.target_temp.saturating_add(cfg.critical_offset) {
            self.below_since = None;
            self.duty = Some(10_000);
            return 10_000;
        }

        let target = self.curve.get_duty(avg).unwrap_or(10_000);

        // Track sustained airflow demand.
        let demand = (sample.cpu_load >= cfg.sustained_load_threshold && avg >= cfg.target_temp)
            || (sample.gpu_temp >= cfg.gpu_temp_threshold);
        if demand {
            self.demand_since.get_or_insert(now);
        } else {
            self.demand_since = None;
        }
        let sustained =
            self.demand_since.map_or(false, |since| now.duration_since(since) >= cfg.sustained_delay);

        // Start from the curve on the first sample so we never under-cool at startup.
        let current = *self.duty.get_or_insert(target.min(cfg.silence_threshold).max(
            if target > cfg.silence_threshold { cfg.silence_threshold } else { target },
        ));

        let next = if target > current {
            self.below_since = None;
            if current < cfg.silence_threshold {
                target.min(cfg.silence_threshold)
            } else if sustained {
                (current + cfg.ramp_step).min(target)
            } else {
                current
            }
        } else if target < current {
            if current - target < cfg.min_decrease {
                self.below_since = None;
                current
            } else {
                let since = *self.below_since.get_or_insert(now);
                if now.duration_since(since) >= cfg.ramp_down_delay {
                    self.below_since = None;
                    target
                } else {
                    current
                }
            }
        } else {
            self.below_since = None;
            current
        };

        self.duty = Some(next);
        next
    }
}

pub fn nvidia_temperatures<F: FnMut(u32)>(func: F) -> io::Result<()> {
    let output = Command::new("nvidia-smi")
        .arg("--query-gpu=temperature.gpu")
        .arg("--format=csv,noheader")
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .stdin(Stdio::null())
        .output()?;

    let stdout = String::from_utf8(output.stdout)
        .map_err(|_| io::Error::new(io::ErrorKind::Other, "non-utf8 output"))?;

    stdout.lines().filter_map(|line| line.parse::<u32>().ok()).for_each(func);

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duty_interpolation() {
        let fan_point = FanPoint::new(20_00, 30_00);
        let next_point = FanPoint::new(30_00, 35_00);

        assert_eq!(fan_point.get_duty_between_points(next_point, 1500), None);
        assert_eq!(fan_point.get_duty_between_points(next_point, 2000), Some(3000));
        assert_eq!(fan_point.get_duty_between_points(next_point, 3000), Some(3500));
        assert_eq!(fan_point.get_duty_between_points(next_point, 3250), None);
        assert_eq!(fan_point.get_duty_between_points(next_point, 3500), None);
    }

    #[test]
    fn standard_points() {
        let standard = FanCurve::standard();

        assert_eq!(standard.get_duty(0), Some(0));
        assert_eq!(standard.get_duty(4499), Some(0));
        assert_eq!(standard.get_duty(4500), Some(3000));
        assert_eq!(standard.get_duty(5500), Some(3500));
        assert_eq!(standard.get_duty(6500), Some(4000));
        assert_eq!(standard.get_duty(7500), Some(5000));
        assert_eq!(standard.get_duty(7800), Some(6000));
        assert_eq!(standard.get_duty(8100), Some(7000));
        assert_eq!(standard.get_duty(8400), Some(8000));
        assert_eq!(standard.get_duty(8600), Some(9000));
        assert_eq!(standard.get_duty(8800), Some(10000));
        assert_eq!(standard.get_duty(10000), Some(10000));
    }

    #[test]
    fn hedt_points() {
        let hedt = FanCurve::hedt();

        assert_eq!(hedt.get_duty(0), Some(3000));
        assert_eq!(hedt.get_duty(5000), Some(3500));
        assert_eq!(hedt.get_duty(6000), Some(4500));
        assert_eq!(hedt.get_duty(7000), Some(5500));
        assert_eq!(hedt.get_duty(7400), Some(6000));
        assert_eq!(hedt.get_duty(7600), Some(7000));
        assert_eq!(hedt.get_duty(7800), Some(8000));
        assert_eq!(hedt.get_duty(8100), Some(10000));
        assert_eq!(hedt.get_duty(10000), Some(10000));
    }

    #[test]
    fn threadripper2_points() {
        let threadripper2 = FanCurve::threadripper2();

        assert_eq!(threadripper2.get_duty(0), Some(3000));
        assert_eq!(threadripper2.get_duty(4000), Some(4000));
        assert_eq!(threadripper2.get_duty(4750), Some(5000));
        assert_eq!(threadripper2.get_duty(5500), Some(6500));
        assert_eq!(threadripper2.get_duty(6250), Some(8500));
        assert_eq!(threadripper2.get_duty(6625), Some(10000));
        assert_eq!(threadripper2.get_duty(10000), Some(10000));
    }

    #[test]
    fn xeon_points() {
        let xeon = FanCurve::xeon();

        assert_eq!(xeon.get_duty(0), Some(4000));
        assert_eq!(xeon.get_duty(5000), Some(4000));
        assert_eq!(xeon.get_duty(5500), Some(4500));
        assert_eq!(xeon.get_duty(6000), Some(5000));
        assert_eq!(xeon.get_duty(6500), Some(5500));
        assert_eq!(xeon.get_duty(7000), Some(6000));
        assert_eq!(xeon.get_duty(7200), Some(6500));
        assert_eq!(xeon.get_duty(7400), Some(8000));
        assert_eq!(xeon.get_duty(7600), Some(8500));
        assert_eq!(xeon.get_duty(7700), Some(9000));
        assert_eq!(xeon.get_duty(7800), Some(10000));
        assert_eq!(xeon.get_duty(10000), Some(10000));
    }

    fn sample(temp: i16, load: f32) -> Sample { Sample { temp, gpu_temp: 0, cpu_load: load } }

    #[test]
    fn pbo_points() {
        let pbo = FanCurve::pbo();
        assert_eq!(pbo.get_duty(0), Some(0));
        assert_eq!(pbo.get_duty(4000), Some(0));
        assert_eq!(pbo.get_duty(5000), Some(2000));
        assert_eq!(pbo.get_duty(6000), Some(4000));
        assert_eq!(pbo.get_duty(7500), Some(7000));
        assert_eq!(pbo.get_duty(9000), Some(10000));
        assert_eq!(pbo.get_duty(10000), Some(10000));
    }

    #[test]
    fn controller_holds_quiet_floor_without_sustained_demand() {
        let mut c = FanController::new(FanCurve::pbo(), FanConfig::default());
        let t0 = Instant::now();
        // 80°C wants 80%, but with no load/demand we stay at the 40% quiet floor.
        for i in 0..30 {
            let d = c.update(sample(80_00, 10.0), t0 + Duration::from_secs(i));
            assert_eq!(d, 4000);
        }
    }

    #[test]
    fn controller_ramps_one_point_per_update_under_sustained_load() {
        let mut c = FanController::new(FanCurve::pbo(), FanConfig::default());
        let t0 = Instant::now();
        let mut last = 0;
        for i in 0..10 {
            last = c.update(sample(91_00, 100.0), t0 + Duration::from_secs(i));
            assert_eq!(last, 4000, "held until demand has been sustained");
        }
        for i in 10..15 {
            let d = c.update(sample(91_00, 100.0), t0 + Duration::from_secs(i));
            assert_eq!(d, last + 100);
            last = d;
        }
    }

    #[test]
    fn controller_critical_override_and_delayed_ramp_down() {
        let mut c = FanController::new(FanCurve::pbo(), FanConfig::default());
        let t0 = Instant::now();
        assert_eq!(c.update(sample(94_00, 100.0), t0), 10_000);

        // Cool down: the temperature average falls but the duty must hold for 10s.
        let mut now = t0;
        let mut d = 10_000;
        for _ in 0..5 {
            now += Duration::from_secs(1);
            d = c.update(sample(50_00, 0.0), now);
        }
        assert_eq!(d, 10_000);
        for _ in 0..20 {
            now += Duration::from_secs(1);
            d = c.update(sample(50_00, 0.0), now);
        }
        assert_eq!(d, 2000);
    }
}
