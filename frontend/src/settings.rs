//! View preferences, remembered per browser.

use crate::store::{DEFAULT_KEEP, KEEP_CHOICES};
use crate::util::window;
use common::layout::{Arrangement, Container, LayoutStyle, NodeStyle};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

const KEY: &str = "wirepulse.view.v1";
/// Key used before the project was renamed; read once so saved
/// preferences carry over.
const OLD_KEY: &str = "hubble-lens.view.v1";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum EdgeStyle {
    #[default]
    Curved,
    Straight,
    Orthogonal,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LabelMode {
    #[default]
    Auto,
    Always,
    Off,
}

/// Columns of the flows table that can be shown or hidden.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Col {
    Time,
    Source,
    SourceIp,
    SourceIdentity,
    Destination,
    DestinationIp,
    DestinationIdentity,
    Port,
    TcpFlags,
    L7,
    Latency,
    Verdict,
    Direction,
    TracePoint,
    Policy,
    Node,
}

impl Col {
    pub const ALL: [Col; 16] = [
        Col::Time,
        Col::Source,
        Col::SourceIp,
        Col::SourceIdentity,
        Col::Destination,
        Col::DestinationIp,
        Col::DestinationIdentity,
        Col::Port,
        Col::TcpFlags,
        Col::L7,
        Col::Latency,
        Col::Verdict,
        Col::Direction,
        Col::TracePoint,
        Col::Policy,
        Col::Node,
    ];

    /// Default width in CSS pixels (the table uses `table-layout: fixed`);
    /// see `ViewSettings::col_width`.
    pub fn width(self) -> u32 {
        match self {
            Col::Time => 118,
            Col::Source => 240,
            Col::SourceIp => 150,
            Col::SourceIdentity => 84,
            Col::Destination => 260,
            Col::DestinationIp => 130,
            Col::DestinationIdentity => 84,
            Col::Port => 92,
            Col::TcpFlags => 92,
            Col::L7 => 300,
            Col::Latency => 80,
            Col::Verdict => 124,
            Col::Direction => 76,
            Col::TracePoint => 116,
            Col::Policy => 200,
            Col::Node => 110,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Col::Time => "Time",
            Col::Source => "Source",
            Col::SourceIp => "Source IP",
            Col::SourceIdentity => "Source identity",
            Col::Destination => "Destination",
            Col::DestinationIp => "Destination IP",
            Col::DestinationIdentity => "Destination identity",
            Col::Port => "Port",
            Col::TcpFlags => "TCP flags",
            Col::L7 => "L7",
            Col::Latency => "Latency",
            Col::Verdict => "Verdict",
            Col::Direction => "Direction",
            Col::TracePoint => "Observation point",
            Col::Policy => "Policy",
            Col::Node => "Node",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ViewSettings {
    pub layout: LayoutStyle,
    pub edges: EdgeStyle,
    pub labels: LabelMode,
    pub icons: bool,
    pub animate: bool,
    /// Perspective 3D view instead of the flat map.
    pub three_d: bool,
    /// 3D style: namespaces stacked as floors (true) or spread on the ground.
    pub tower: bool,
    /// Slowly turn the 3D view while nobody touches it.
    pub auto_rotate: bool,
    /// Hide map nodes and connections that don't match the filter.
    pub filter_map: bool,
    pub drawer_open: bool,
    /// Drawer height in CSS pixels.
    pub drawer_height: f64,
    pub columns: Vec<Col>,
    /// Column widths dragged to another size; the rest use `Col::width`.
    #[serde(deserialize_with = "known_columns")]
    pub col_widths: BTreeMap<Col, u32>,
    /// Flows each table keeps (see `store`).
    pub keep_flows: usize,
    /// Width of the details panel beside the flows and endpoints tables.
    pub side_width: f64,
    /// Width of the map's details panel.
    pub details_width: f64,
}

pub const SIDE_WIDTH: f64 = 380.0;
pub const DETAILS_WIDTH: f64 = 340.0;

impl ViewSettings {
    pub fn col_width(&self, c: Col) -> u32 {
        self.col_widths.get(&c).copied().unwrap_or_else(|| c.width())
    }

    /// `None` goes back to the default width.
    pub fn set_col_width(&mut self, c: Col, width: Option<f64>) {
        match width {
            Some(w) => self.col_widths.insert(c, w as u32),
            None => self.col_widths.remove(&c),
        };
    }
}

/// Column widths as saved. Columns this version doesn't know are skipped:
/// failing would reset every setting.
fn known_columns<'de, D: Deserializer<'de>>(d: D) -> Result<BTreeMap<Col, u32>, D::Error> {
    let saved = BTreeMap::<String, u32>::deserialize(d)?;
    Ok(saved
        .into_iter()
        .filter_map(|(k, w)| serde_json::from_value::<Col>(serde_json::Value::String(k)).ok().map(|c| (c, w)))
        .collect())
}

impl Default for ViewSettings {
    fn default() -> Self {
        let narrow = window().inner_width().ok().and_then(|w| w.as_f64()).is_some_and(|w| w < 760.0);
        Self {
            layout: LayoutStyle::default(),
            edges: EdgeStyle::Curved,
            labels: LabelMode::Auto,
            icons: true,
            animate: true,
            three_d: false,
            tower: false,
            auto_rotate: true,
            filter_map: true,
            // Phones start with the map; the panel is one tap away.
            drawer_open: !narrow,
            drawer_height: 280.0,
            columns: vec![
                Col::Time,
                Col::Source,
                Col::Destination,
                Col::Port,
                Col::TcpFlags,
                Col::L7,
                Col::Verdict,
                Col::Policy,
            ],
            col_widths: BTreeMap::new(),
            keep_flows: DEFAULT_KEEP,
            side_width: SIDE_WIDTH,
            details_width: DETAILS_WIDTH,
        }
    }
}

/// Named combinations offered at the top of the View menu.
pub const PRESETS: &[(&str, LayoutStyle, EdgeStyle)] = &[
    (
        "Bubbles",
        LayoutStyle { node: NodeStyle::Circle, container: Container::Circle, arrangement: Arrangement::Packed },
        EdgeStyle::Curved,
    ),
    (
        "Boxes",
        LayoutStyle { node: NodeStyle::Circle, container: Container::Box, arrangement: Arrangement::Packed },
        EdgeStyle::Orthogonal,
    ),
    (
        "Cards",
        LayoutStyle { node: NodeStyle::Card, container: Container::Box, arrangement: Arrangement::Packed },
        EdgeStyle::Curved,
    ),
    (
        "Flow",
        LayoutStyle { node: NodeStyle::Card, container: Container::Box, arrangement: Arrangement::Layered },
        EdgeStyle::Curved,
    ),
];

/// Browser storage can be missing or throw (private mode, sandboxed frames);
/// every failure just means "use defaults".
fn storage() -> Option<web_sys::Storage> {
    window().local_storage().ok().flatten()
}

pub fn load() -> ViewSettings {
    let s = storage();
    let mut v: ViewSettings = s
        .as_ref()
        .and_then(|s| s.get_item(KEY).ok().flatten())
        .or_else(|| s.as_ref().and_then(|s| s.get_item(OLD_KEY).ok().flatten()))
        .and_then(|j| serde_json::from_str(&j).ok())
        .unwrap_or_default();
    // The nearest choice the menu offers.
    v.keep_flows = KEEP_CHOICES.into_iter().min_by_key(|c| c.abs_diff(v.keep_flows)).unwrap_or(DEFAULT_KEEP);
    v
}

pub fn save(v: &ViewSettings) {
    if let (Some(s), Ok(j)) = (storage(), serde_json::to_string(v)) {
        let _ = s.set_item(KEY, &j);
    }
}
