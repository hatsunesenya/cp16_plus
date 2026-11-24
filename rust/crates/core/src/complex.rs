//! 极简复数类型：只为避免引入 num-complex，接口对齐 numpy 的用法。

use std::ops::{Add, Mul, Sub};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct C64 {
    pub re: f64,
    pub im: f64,
}

impl C64 {
    pub const ZERO: C64 = C64 { re: 0.0, im: 0.0 };
    pub const ONE: C64 = C64 { re: 1.0, im: 0.0 };

    #[inline]
    pub fn new(re: f64, im: f64) -> Self {
        C64 { re, im }
    }

    #[inline]
    pub fn conj(self) -> Self {
        C64::new(self.re, -self.im)
    }

    #[inline]
    pub fn norm2(self) -> f64 {
        self.re * self.re + self.im * self.im
    }

    #[inline]
    pub fn abs(self) -> f64 {
        self.norm2().sqrt()
    }

    #[inline]
    pub fn arg(self) -> f64 {
        self.im.atan2(self.re)
    }

    #[inline]
    pub fn exp_i(theta: f64) -> Self {
        C64::new(theta.cos(), theta.sin())
    }

    #[inline]
    pub fn scale(self, s: f64) -> Self {
        C64::new(self.re * s, self.im * s)
    }
}

impl Add for C64 {
    type Output = C64;
    #[inline]
    fn add(self, o: C64) -> C64 {
        C64::new(self.re + o.re, self.im + o.im)
    }
}

impl Sub for C64 {
    type Output = C64;
    #[inline]
    fn sub(self, o: C64) -> C64 {
        C64::new(self.re - o.re, self.im - o.im)
    }
}

impl Mul for C64 {
    type Output = C64;
    #[inline]
    fn mul(self, o: C64) -> C64 {
        C64::new(self.re * o.re - self.im * o.im,
                 self.re * o.im + self.im * o.re)
    }
}

impl Mul<f64> for C64 {
    type Output = C64;
    #[inline]
    fn mul(self, s: f64) -> C64 {
        self.scale(s)
    }
}
