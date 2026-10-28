//! Research values on the wire: the channel names.

use serde::de::DeserializeOwned;
use serde::Serialize;

use crate::*;

fn round_trip<T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug>(v: &T) -> String {
    let s = serde_json::to_string(v).unwrap();
    let back: T = serde_json::from_str(&s).unwrap();
    assert_eq!(&back, v, "round trip changed the value: {s}");
    s
}

#[test]
fn channels_have_stable_names() {
    for c in ChannelId::ALL {
        assert_eq!(round_trip(&c), format!("\"{}\"", c.as_str()));
        assert_eq!(c.as_str().parse::<ChannelId>().unwrap(), c);
    }
    assert_eq!("S2".parse::<ChannelId>().unwrap(), ChannelId::SemanticScholar);
    assert_eq!("searxng".parse::<ChannelId>().unwrap(), ChannelId::Web);
    assert_eq!("Hacker-News".parse::<ChannelId>().unwrap(), ChannelId::HackerNews);
    assert!("twitter".parse::<ChannelId>().is_err());
    assert!(!ChannelId::Memory.is_network() && ChannelId::Web.is_network());
}
