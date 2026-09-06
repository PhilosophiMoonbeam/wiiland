//! Allocation-free, integer calibration windows for motion sensors.

use crate::config::SensorCalibration;

pub const AIM_CALIBRATION_MIN_SAMPLES: u32 = 16;
pub const AIM_CALIBRATION_MAX_JITTER: i32 = 512;
pub const SENSOR_CALIBRATION_X: u8 = 1 << 0;
pub const SENSOR_CALIBRATION_Y: u8 = 1 << 1;
pub const SENSOR_CALIBRATION_Z: u8 = 1 << 2;
pub const SENSOR_CALIBRATION_ALL: u8 =
    SENSOR_CALIBRATION_X | SENSOR_CALIBRATION_Y | SENSOR_CALIBRATION_Z;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CalibrationStats {
    pub samples: u32,
    pub sum_x: i64,
    pub sum_y: i64,
    pub sum_z: i64,
    pub min_x: i32,
    pub min_y: i32,
    pub min_z: i32,
    pub max_x: i32,
    pub max_y: i32,
    pub max_z: i32,
}
impl Default for CalibrationStats {
    fn default() -> Self {
        Self::new()
    }
}
impl CalibrationStats {
    pub const fn new() -> Self {
        Self {
            samples: 0,
            sum_x: 0,
            sum_y: 0,
            sum_z: 0,
            min_x: i32::MAX,
            min_y: i32::MAX,
            min_z: i32::MAX,
            max_x: i32::MIN,
            max_y: i32::MIN,
            max_z: i32::MIN,
        }
    }
    pub fn clear(&mut self) {
        *self = Self::new();
    }
    pub fn add(&mut self, sample: [i32; 3]) {
        self.samples = self.samples.saturating_add(1);
        self.sum_x += i64::from(sample[0]);
        self.sum_y += i64::from(sample[1]);
        self.sum_z += i64::from(sample[2]);
        self.min_x = self.min_x.min(sample[0]);
        self.min_y = self.min_y.min(sample[1]);
        self.min_z = self.min_z.min(sample[2]);
        self.max_x = self.max_x.max(sample[0]);
        self.max_y = self.max_y.max(sample[1]);
        self.max_z = self.max_z.max(sample[2]);
    }
    pub fn jitter(&self) -> i32 {
        if self.samples == 0 {
            return 0;
        }
        (i64::from(self.max_x) - i64::from(self.min_x))
            .max(i64::from(self.max_y) - i64::from(self.min_y))
            .max(i64::from(self.max_z) - i64::from(self.min_z))
            .min(i64::from(i32::MAX)) as i32
    }
    pub fn finish(&self) -> Option<SensorCalibration> {
        if self.samples < AIM_CALIBRATION_MIN_SAMPLES || self.jitter() > AIM_CALIBRATION_MAX_JITTER
        {
            return None;
        }
        Some(SensorCalibration {
            axes: SENSOR_CALIBRATION_ALL,
            x: (self.sum_x / i64::from(self.samples)) as i32,
            y: (self.sum_y / i64::from(self.samples)) as i32,
            z: (self.sum_z / i64::from(self.samples)) as i32,
        })
    }
}
pub fn sensor_calibration_ready(calibration: &SensorCalibration) -> bool {
    calibration.axes == SENSOR_CALIBRATION_ALL
}
pub fn calibration_jitter(stats: &CalibrationStats) -> i32 {
    stats.jitter()
}
pub fn calibration_stats_finish(stats: &CalibrationStats) -> Option<SensorCalibration> {
    stats.finish()
}
