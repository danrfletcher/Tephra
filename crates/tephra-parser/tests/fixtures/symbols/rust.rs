//! crate docs
use std::fmt;

/// A thing.
#[derive(Debug, Clone)]
pub struct Thing {
    pub a: u8,
}

/// Why.
#[derive(Debug)]
pub enum Reason {
    One,
    Two,
}

pub trait Shape {
    const SIDES: u32 = 0;
    fn area(&self) -> f64;
    fn name(&self) -> String {
        String::new()
    }
}

impl Reason {
    pub fn code(&self) -> u8 {
        fn helper() {}
        let closure = || 1;
        0
    }
}

impl<T> fmt::Display for Reason where T: Copy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        Ok(())
    }
}

pub mod inner {
    pub const LIMIT: usize = 3;
    pub static NAME: &str = "x";
    pub fn deep() {}
}

mod declared;

pub const TOP: u8 = 1;
static COUNTER: u8 = 0;
type Alias = u8;

pub fn free() {}
