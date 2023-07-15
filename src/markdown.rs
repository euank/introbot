// This file taken from https://github.com/andreasots/eris/blob/9e0f0c922c07db8bb98e39fb20f98cd537f252c1/src/markdown.rs
// Used under the terms of the apache 2.0 license. You may get a copy of the license here:
// https://github.com/andreasots/eris/blob/9e0f0c922c07db8bb98e39fb20f98cd537f252c1/LICENSE

use std::borrow::Cow;

use regex::{Captures, Regex};

pub fn escape(text: &str) -> Cow<str> {
    lazy_static::lazy_static! {
        static ref RE_META: Regex = Regex::new(r"(https?://\S+)|([_`*~|])").unwrap();
    }

    RE_META.replace_all(text, |caps: &Captures| {
        if let Some(m) = caps.get(1) {
            format!("<{}>", m.as_str())
        } else if let Some(m) = caps.get(2) {
            format!("\\{}", m.as_str())
        } else {
            unreachable!()
        }
    })
}
