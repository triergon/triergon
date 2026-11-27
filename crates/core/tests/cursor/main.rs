//! `InputCursor`: requests and their answers, the four reads, what a window
//! keeps charged, and the blocking feed.
#![forbid(unsafe_code)]

mod append;
mod discard;
mod feed;
mod inline;
mod owned;
mod retention;
mod support;
mod windows;
