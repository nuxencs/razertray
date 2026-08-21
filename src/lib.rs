#![cfg_attr(not(target_os = "windows"), allow(dead_code))]

pub(crate) const APP_ID: &str = "razertray";

pub mod app;
mod application;
mod autostart;
mod config;
mod device_map;
mod error_tracker;
mod forecast;
mod hid;
mod icon;
mod model;
mod notify;
mod single_instance;
mod tray;
