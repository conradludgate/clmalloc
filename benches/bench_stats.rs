#[derive(Default)]
pub struct RunStats {
    values: Vec<f64>,
}

impl RunStats {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, value: f64) {
        self.values.push(value);
    }

    pub fn median(&self) -> f64 {
        let mut sorted = self.values.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        let n = sorted.len();
        if n.is_multiple_of(2) {
            (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
        } else {
            sorted[n / 2]
        }
    }

    pub fn mean(&self) -> f64 {
        self.values.iter().sum::<f64>() / self.values.len() as f64
    }

    pub fn stddev(&self) -> f64 {
        let mean = self.mean();
        let variance =
            self.values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / self.values.len() as f64;
        variance.sqrt()
    }

    pub fn min(&self) -> f64 {
        self.values.iter().cloned().reduce(f64::min).unwrap()
    }

    pub fn max(&self) -> f64 {
        self.values.iter().cloned().reduce(f64::max).unwrap()
    }

    pub fn print(&self, label: &str, unit: &str) {
        let n = self.values.len();
        if n == 1 {
            println!("  {label}:   {:.0} {unit}", self.values[0]);
            return;
        }
        println!(
            "  {label}:   {:.0} ± {:.0} {unit}  (median {:.0}, min {:.0}, max {:.0}, n={n})",
            self.mean(),
            self.stddev(),
            self.median(),
            self.min(),
            self.max(),
        );
    }
}
