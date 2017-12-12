// Copyright © 2016 Felix Obenhuber
// This program is free software. It comes without any warranty, to the extent
// permitted by applicable law. You can redistribute it and/or modify it under
// the terms of the Do What The Fuck You Want To Public License, Version 2, as
// published by Sam Hocevar. See the COPYING file for more details.

use clap::ArgMatches;
use failure::Error;
use futures::{Async, AsyncSink, Poll, Sink, StartSend};
use profiles::*;
use regex::Regex;
use std::cmp::max;
use std::collections::HashMap;
use std::io::Write;
use std::io::stdout;
use std::str::FromStr;
use super::config_get;
use super::record::{Format, Level, Record};
use term_painter::Attr::*;
use term_painter::{Color, Style, ToStyle};
use time::Tm;
use utils::terminal_width;

#[cfg(not(target_os = "windows"))]
pub const DIMM_COLOR: Color = Color::Custom(243);
#[cfg(target_os = "windows")]
pub const DIMM_COLOR: Color = Color::White;

pub struct Terminal {
    beginning_of: Regex,
    color: bool,
    date_format: (String, usize),
    diff_width: usize,
    format: Format,
    highlight: Vec<Regex>,
    no_dimm: bool,
    process_width: usize,
    shorten_tag: bool,
    tag_timestamps: HashMap<String, Tm>,
    tag_width: Option<usize>,
    thread_width: usize,
    time_diff: bool,
    vovels: Regex,
}

impl<'a> Terminal {
    pub fn new(args: &ArgMatches<'a>, profile: &Profile) -> Result<Self, Error> {
        let mut hl = profile.highlight().clone();
        if args.is_present("highlight") {
            hl.extend(values_t!(args.values_of("highlight"), String)?);
        }
        let highlight = hl.iter().flat_map(|h| Regex::new(h)).collect();

        let format = args.value_of("format")
            .and_then(|f| Format::from_str(f).ok())
            .unwrap_or(Format::Human);
        if format == Format::Html {
            return Err(format_err!(
                "HTML format is unsupported when writing to files"
            ));
        }

        let color =
            !args.is_present("monochrome") && !config_get("terminal_monochrome").unwrap_or(false);
        let hide_timestamp = args.is_present("hide_timestamp")
            || config_get("terminal_hide_timestamp").unwrap_or(false);
        let no_dimm = args.is_present("no_dimm") || config_get("terminal_no_dimm").unwrap_or(false);
        let shorten_tag =
            args.is_present("shorten_tags") || config_get("terminal_shorten_tags").unwrap_or(false);
        let show_date =
            args.is_present("show_date") || config_get("terminal_show_date").unwrap_or(false);
        let tag_width = config_get("terminal_tag_width");
        let time_diff = args.is_present("show_time_diff")
            || config_get("terminal_show_time_diff").unwrap_or(false);
        let time_diff_width = config_get("terminal_time_diff_width").unwrap_or(8);

        Ok(Terminal {
            beginning_of: Regex::new(r"--------- beginning of.*").unwrap(),
            color,
            date_format: if show_date {
                if hide_timestamp {
                    ("%m-%d".to_owned(), 5)
                } else {
                    ("%m-%d %H:%M:%S.%f".to_owned(), 18)
                }
            } else if hide_timestamp {
                ("".to_owned(), 0)
            } else {
                ("%H:%M:%S.%f".to_owned(), 12)
            },
            format,
            highlight,
            shorten_tag,
            no_dimm,
            process_width: 0,
            tag_timestamps: HashMap::new(),
            vovels: Regex::new(r"a|e|i|o|u").unwrap(),
            tag_width,
            thread_width: 0,
            diff_width: if time_diff { time_diff_width } else { 0 },
            time_diff,
        })
    }

    /// Filter some unreadable (on dark background) or nasty colors
    fn hashed_color(item: &str) -> Color {
        match item.bytes().fold(42u16, |c, x| c ^ u16::from(x)) {
            c @ 0...1 => Color::Custom(c + 2),
            c @ 16...21 => Color::Custom(c + 6),
            c @ 52...55 | c @ 126...129 => Color::Custom(c + 4),
            c @ 163...165 | c @ 200...201 => Color::Custom(c + 3),
            c @ 207 => Color::Custom(c + 1),
            c @ 232...240 => Color::Custom(c + 9),
            c => Color::Custom(c),
        }
    }

    fn print_record(&mut self, record: &Record) -> Result<(), Error> {
        match self.format {
            Format::Csv | Format::Json | Format::Raw => {
                println!("{}", record.format(&self.format)?);
                Ok(())
            }
            Format::Human => self.print_human(record),
            Format::Html => {
                unreachable!("Unimplemented format html");
            }
        }
    }

    fn highlight_style(&self, s: &str, c: Color, h: &mut bool) -> Style {
        if self.highlight.iter().any(|r| r.is_match(s)) {
            *h = true;
            Bold.fg(c)
        } else {
            Plain.fg(c)
        }
    }

    // TODO
    // Rework this to use a more column based approach!
    fn print_human(&mut self, record: &Record) -> Result<(), Error> {
        let (timestamp, mut diff) = if let Some(ts) = record.timestamp.clone() {
            let ts = *ts;
            let timestamp = match ::time::strftime(&self.date_format.0, &ts) {
                Ok(t) => t.chars().take(self.date_format.1).collect::<String>(),
                Err(_) => (0..self.date_format.1).map(|_| " ").collect::<String>(),
            };

            let diff = if self.time_diff {
                if let Some(t) = self.tag_timestamps.get(&record.tag) {
                    let diff = ((ts - *t).num_milliseconds()).abs();
                    let diff = format!("{}.{:.03}", diff / 1000, diff % 1000);
                    if diff.chars().count() <= self.diff_width {
                        diff
                    } else {
                        "-.---".to_owned()
                    }
                } else {
                    "".to_owned()
                }
            } else {
                "".to_owned()
            };

            (timestamp, diff)
        } else {
            ("".to_owned(), "".to_owned())
        };

        let terminal_width = terminal_width();
        let tag_width = self.tag_width.unwrap_or_else(|| match terminal_width {
            Some(n) if n <= 80 => 15,
            Some(n) if n <= 90 => 20,
            Some(n) if n <= 100 => 25,
            Some(n) if n <= 110 => 30,
            _ => 35,
        });

        let tag = {
            let mut t = if self.beginning_of.is_match(&record.message) {
                diff = "".to_owned();
                self.tag_timestamps.clear();
                // Print horizontal line if temrinal width is detectable
                if let Some(width) = terminal_width {
                    println!("{}", (0..width).map(|_| "─").collect::<String>());
                }
                // "beginnig of" messages never have a tag
                record.message.clone()
            } else {
                record.tag.clone()
            };

            if t.chars().count() > tag_width {
                if self.shorten_tag {
                    t = self.vovels.replace_all(&t.to_owned(), "").to_string();
                }
                if t.chars().count() > tag_width {
                    t.truncate(tag_width);
                }
            }
            format!("{:>width$}", t, width = tag_width)
        };

        self.process_width = max(self.process_width, record.process.chars().count());
        let pid = if record.process.is_empty() {
            " ".repeat(self.process_width)
        } else {
            format!("{:<width$}", record.process, width = self.process_width)
        };
        let tid = if record.thread.is_empty() {
            if self.thread_width > 0 {
                " ".repeat(self.thread_width + 1)
            } else {
                "".to_owned()
            }
        } else {
            self.thread_width = max(self.thread_width, record.thread.chars().count());
            format!(" {:>width$}", record.thread, width = self.thread_width)
        };

        let dimm_color = if self.no_dimm {
            Color::White
        } else {
            DIMM_COLOR
        };

        let level = format!(" {} ", record.level);
        let level_color = match record.level {
            Level::Trace | Level::Verbose | Level::Debug | Level::None => dimm_color,
            Level::Info => Color::Green,
            Level::Warn => Color::Yellow,
            Level::Error | Level::Fatal | Level::Assert => Color::Red,
        };

        let mut highlight = false;
        let color = self.color;
        let diff_width = self.diff_width;
        let timestamp_width = self.date_format.1;
        let msg_style = self.highlight_style(&record.message, level_color, &mut highlight);
        let tag_style = self.highlight_style(&tag, Self::hashed_color(&tag), &mut highlight);
        let pid_style = self.highlight_style(&pid, Self::hashed_color(&pid), &mut highlight);
        let tid_style = self.highlight_style(&tid, Self::hashed_color(&tid), &mut highlight);
        let level_style = Plain.bg(level_color).fg(Color::Black);
        let timestamp_style = if highlight {
            Bold.fg(Color::Yellow)
        } else {
            Plain.fg(dimm_color)
        };

        let print_msg = |chunk: &str, sign: &str| {
            if color {
                println!(
                    "{:<timestamp_width$} {:>diff_width$} {:>tag_width$} ({}{}) {} {} {}",
                    timestamp_style.paint(&timestamp),
                    dimm_color.paint(&diff),
                    tag_style.paint(&tag),
                    pid_style.paint(&pid),
                    tid_style.paint(&tid),
                    level_style.paint(&level),
                    level_color.paint(sign),
                    msg_style.paint(&chunk),
                    timestamp_width = timestamp_width,
                    diff_width = diff_width,
                    tag_width = tag_width
                );
            } else {
                println!(
                    "{:<timestamp_width$} {:>diff_width$} {:>tag_width$} ({}{}) {} {} {}",
                    timestamp,
                    diff,
                    tag,
                    pid,
                    tid,
                    level,
                    sign,
                    chunk,
                    timestamp_width = timestamp_width,
                    diff_width = diff_width,
                    tag_width = tag_width
                );
            }
        };

        if let Some(width) = terminal_width {
            let preamble_width =
                timestamp_width + 1 + self.diff_width + 1 + tag_width + 1 + 1 + self.process_width
                    + if self.thread_width == 0 { 0 } else { 1 } + self.thread_width
                    + 1 + 1 + 3 + 3;
            // Windows terminal width reported is too big
            #[cfg(target_os = "windows")]
            let preamble_width = preamble_width + 1;

            let record_len = record.message.chars().count();
            let columns = width as usize;
            if (preamble_width + record_len) > columns {
                let mut m = record.message.clone();
                // TODO: Refactor this!
                while !m.is_empty() {
                    let chars_left = m.chars().count();
                    let (chunk_width, sign) = if chars_left == record_len {
                        (columns - preamble_width, "┌")
                    } else if chars_left <= (columns - preamble_width) {
                        (chars_left, "└")
                    } else {
                        (columns - preamble_width, "├")
                    };

                    let chunk: String = m.chars().take(chunk_width).collect();
                    m = m.chars().skip(chunk_width).collect();
                    if self.color {
                        let c = level_color.paint(chunk).to_string();
                        print_msg(&c, sign)
                    } else {
                        print_msg(&chunk, sign)
                    }
                }
            } else {
                print_msg(&record.message, " ");
            }
        } else {
            print_msg(&record.message, " ");
        };

        if let Some(ts) = record.timestamp.clone() {
            if self.time_diff && !record.tag.is_empty() {
                self.tag_timestamps.insert(record.tag.clone(), *ts);
            }
        }

        stdout().flush().map_err(|e| e.into())
    }
}

impl Sink for Terminal {
    type SinkItem = Option<Record>;
    type SinkError = Error;

    fn start_send(&mut self, item: Self::SinkItem) -> StartSend<Self::SinkItem, Self::SinkError> {
        if let Some(record) = item {
            if let Err(e) = self.print_record(&record) {
                return Err(e);
            }
        }
        Ok(AsyncSink::Ready)
    }

    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        Ok(Async::Ready(()))
    }
}
