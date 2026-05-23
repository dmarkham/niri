use knuffel::errors::DecodeError;

use crate::binds::Action;
use crate::utils::MergeWith;
use crate::FloatOrInt;

#[derive(Debug, Default, Clone, PartialEq)]
pub struct Gestures {
    pub dnd_edge_view_scroll: DndEdgeViewScroll,
    pub dnd_edge_workspace_switch: DndEdgeWorkspaceSwitch,
    pub hot_corners: HotCorners,
    pub hot_edges: Vec<HotEdge>,
}

#[derive(knuffel::Decode, Debug, Default, Clone, PartialEq)]
pub struct GesturesPart {
    #[knuffel(child)]
    pub dnd_edge_view_scroll: Option<DndEdgeViewScrollPart>,
    #[knuffel(child)]
    pub dnd_edge_workspace_switch: Option<DndEdgeWorkspaceSwitchPart>,
    #[knuffel(child)]
    pub hot_corners: Option<HotCorners>,
    #[knuffel(child)]
    pub hot_edges: Option<HotEdges>,
}

impl MergeWith<GesturesPart> for Gestures {
    fn merge_with(&mut self, part: &GesturesPart) {
        merge!(
            (self, part),
            dnd_edge_view_scroll,
            dnd_edge_workspace_switch,
        );
        merge_clone!((self, part), hot_corners);
        if let Some(edges) = &part.hot_edges {
            self.hot_edges.clone_from(&edges.0);
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DndEdgeViewScroll {
    pub trigger_width: f64,
    pub delay_ms: u16,
    pub max_speed: f64,
}

impl Default for DndEdgeViewScroll {
    fn default() -> Self {
        Self {
            trigger_width: 30., // Taken from GTK 4.
            delay_ms: 100,
            max_speed: 1500.,
        }
    }
}

#[derive(knuffel::Decode, Debug, Clone, Copy, PartialEq)]
pub struct DndEdgeViewScrollPart {
    #[knuffel(child, unwrap(argument))]
    pub trigger_width: Option<FloatOrInt<0, 65535>>,
    #[knuffel(child, unwrap(argument))]
    pub delay_ms: Option<u16>,
    #[knuffel(child, unwrap(argument))]
    pub max_speed: Option<FloatOrInt<0, 1_000_000>>,
}

impl MergeWith<DndEdgeViewScrollPart> for DndEdgeViewScroll {
    fn merge_with(&mut self, part: &DndEdgeViewScrollPart) {
        merge!((self, part), trigger_width, max_speed);
        merge_clone!((self, part), delay_ms);
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DndEdgeWorkspaceSwitch {
    pub trigger_height: f64,
    pub delay_ms: u16,
    pub max_speed: f64,
}

impl Default for DndEdgeWorkspaceSwitch {
    fn default() -> Self {
        Self {
            trigger_height: 50.,
            delay_ms: 100,
            max_speed: 1500.,
        }
    }
}

#[derive(knuffel::Decode, Debug, Clone, Copy, PartialEq)]
pub struct DndEdgeWorkspaceSwitchPart {
    #[knuffel(child, unwrap(argument))]
    pub trigger_height: Option<FloatOrInt<0, 65535>>,
    #[knuffel(child, unwrap(argument))]
    pub delay_ms: Option<u16>,
    #[knuffel(child, unwrap(argument))]
    pub max_speed: Option<FloatOrInt<0, 1_000_000>>,
}

impl MergeWith<DndEdgeWorkspaceSwitchPart> for DndEdgeWorkspaceSwitch {
    fn merge_with(&mut self, part: &DndEdgeWorkspaceSwitchPart) {
        merge!((self, part), trigger_height, max_speed);
        merge_clone!((self, part), delay_ms);
    }
}

#[derive(knuffel::Decode, Debug, Default, Clone, Copy, PartialEq)]
pub struct HotCorners {
    #[knuffel(child)]
    pub off: bool,
    #[knuffel(child)]
    pub top_left: bool,
    #[knuffel(child)]
    pub top_right: bool,
    #[knuffel(child)]
    pub bottom_left: bool,
    #[knuffel(child)]
    pub bottom_right: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotEdgeDirection {
    Top,
    Bottom,
    Left,
    Right,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HotEdge {
    pub direction: HotEdgeDirection,
    pub output: String,
    pub delay_ms: u16,
    pub action: Action,
}

/// Wrapper for parsing the `hot-edges` block from KDL config.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct HotEdges(pub Vec<HotEdge>);

impl<S> knuffel::Decode<S> for HotEdges
where
    S: knuffel::traits::ErrorSpan,
{
    fn decode_node(
        node: &knuffel::ast::SpannedNode<S>,
        ctx: &mut knuffel::decode::Context<S>,
    ) -> Result<Self, DecodeError<S>> {
        let mut edges = Vec::new();

        for child in node.children() {
            let direction = match &**child.node_name {
                "top" => HotEdgeDirection::Top,
                "bottom" => HotEdgeDirection::Bottom,
                "left" => HotEdgeDirection::Left,
                "right" => HotEdgeDirection::Right,
                name => {
                    ctx.emit_error(DecodeError::unexpected(
                        &child.node_name,
                        "node",
                        format!(
                            "unexpected edge direction `{}`; expected top, bottom, left, or right",
                            name.escape_default()
                        ),
                    ));
                    continue;
                }
            };

            let mut output = None;
            let mut delay_ms = 250u16;
            let mut action = None;

            for grandchild in child.children() {
                match &**grandchild.node_name {
                    "output" => {
                        if let Some(arg) = grandchild.arguments.first() {
                            output = Some(knuffel::traits::DecodeScalar::decode(arg, ctx)?);
                        } else {
                            ctx.emit_error(DecodeError::missing(
                                grandchild,
                                "output requires a name argument",
                            ));
                        }
                    }
                    "delay-ms" => {
                        if let Some(arg) = grandchild.arguments.first() {
                            delay_ms = knuffel::traits::DecodeScalar::decode(arg, ctx)?;
                        } else {
                            ctx.emit_error(DecodeError::missing(
                                grandchild,
                                "delay-ms requires a value argument",
                            ));
                        }
                    }
                    _ => {
                        // Try parsing as an action.
                        match Action::decode_node(grandchild, ctx) {
                            Ok(a) => {
                                if action.is_some() {
                                    ctx.emit_error(DecodeError::unexpected(
                                        grandchild,
                                        "node",
                                        "only one action is allowed per hot-edge",
                                    ));
                                } else {
                                    action = Some(a);
                                }
                            }
                            Err(e) => ctx.emit_error(e),
                        }
                    }
                }
            }

            let Some(output_name) = output else {
                ctx.emit_error(DecodeError::missing(child, "hot-edge requires an output"));
                continue;
            };

            let Some(action) = action else {
                ctx.emit_error(DecodeError::missing(child, "hot-edge requires an action"));
                continue;
            };

            edges.push(HotEdge {
                direction,
                output: output_name,
                delay_ms,
                action,
            });
        }

        Ok(Self(edges))
    }
}
