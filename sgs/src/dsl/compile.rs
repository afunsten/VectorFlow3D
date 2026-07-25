//! Semantic analysis: resolve a parsed [`Scene`](super::ast::Scene) against the
//! LSG and lower it into stable-id [`Opinion`]s.
//!
//! Selector resolution reuses [`Lsg::resolve_selector`] (asset tag or prim
//! path). Unresolved selectors and dangling pipe anchors are reported at their
//! exact source span — the DSL is meant to be hand-authored (spec §3.9).

use std::collections::{HashMap, HashSet};

use super::ast::Scene;
use super::diag::Diagnostic;
use super::{lexer, parser};
use crate::lsg::{AnchorId, Anchor, Edge, EdgeId, EntityId, Lsg, TelemetryBinding};
use crate::opinion::Opinion;

pub struct CompileResult {
    pub scene_name: String,
    pub opinions: Vec<Opinion>,
    pub diagnostics: Vec<Diagnostic>,
}

impl CompileResult {
    pub fn has_errors(&self) -> bool {
        self.diagnostics.iter().any(|d| d.is_error())
    }

    pub fn error_count(&self) -> usize {
        self.diagnostics.iter().filter(|d| d.is_error()).count()
    }
}

/// Compile Flow3D `source` (named `filename` for diagnostics) against the LSG.
pub fn compile(source: &str, lsg: &Lsg) -> CompileResult {
    let (tokens, lex_diags) = lexer::lex(source);
    let (scene, mut diagnostics) = parser::parse(tokens, lex_diags);

    let Some(scene) = scene else {
        return CompileResult {
            scene_name: String::new(),
            opinions: Vec::new(),
            diagnostics,
        };
    };

    let mut opinions = Vec::new();
    lower_scene(&scene, lsg, &mut opinions, &mut diagnostics);

    CompileResult {
        scene_name: scene.name,
        opinions,
        diagnostics,
    }
}

fn lower_scene(
    scene: &Scene,
    lsg: &Lsg,
    opinions: &mut Vec<Opinion>,
    diagnostics: &mut Vec<Diagnostic>,
) {
    // Track (entity -> declared anchor names) so pipes can be validated.
    let mut anchors_by_entity: HashMap<EntityId, HashSet<String>> = HashMap::new();
    // Resolve part selectors once for reuse by pipes.
    let mut resolved: HashMap<String, EntityId> = HashMap::new();

    for part in &scene.parts {
        let Some(entity) = lsg.resolve_selector(&part.selector) else {
            diagnostics.push(Diagnostic::error(
                format!("unknown part `{}` (no matching asset tag or prim path in the scene)", part.selector),
                part.selector_span,
            ));
            continue;
        };
        let id = entity.id;
        resolved.insert(part.selector.clone(), id);

        for tag in &part.tags {
            opinions.push(Opinion::Tag {
                entity: id,
                tag: tag.value.clone(),
            });
        }
        for meta in &part.metas {
            opinions.push(Opinion::Meta {
                entity: id,
                key: meta.key.clone(),
                value: meta.value.clone(),
            });
        }
        for anchor in &part.anchors {
            anchors_by_entity
                .entry(id)
                .or_default()
                .insert(anchor.name.clone());
            opinions.push(Opinion::Anchor(Anchor {
                id: AnchorId::new(id, &anchor.name),
                entity: id,
                name: anchor.name.clone(),
                pos: anchor.pos,
            }));
        }
        for b in &part.bindings {
            opinions.push(Opinion::Binding {
                entity: id,
                binding: TelemetryBinding {
                    attribute: b.attribute.clone(),
                    source_id: "victoriametrics".to_string(),
                    query: b.query.clone(),
                    unit: b.unit.clone().unwrap_or_default(),
                    ttl_ms: b.ttl_ms.unwrap_or(5000.0),
                    priority: b.priority.clone().unwrap_or_else(|| "background".to_string()),
                    quality_policy: "stale_ok".to_string(),
                },
            });
        }
    }

    for pipe in &scene.pipes {
        let from = resolve_endpoint(&pipe.from.part, lsg, &resolved);
        let to = resolve_endpoint(&pipe.to.part, lsg, &resolved);
        let (Some(from_id), Some(to_id)) = (from, to) else {
            if from.is_none() {
                diagnostics.push(Diagnostic::error(
                    format!("unknown part `{}` in pipe", pipe.from.part),
                    pipe.from.span,
                ));
            }
            if to.is_none() {
                diagnostics.push(Diagnostic::error(
                    format!("unknown part `{}` in pipe", pipe.to.part),
                    pipe.to.span,
                ));
            }
            continue;
        };

        // Warn (not error) on a pipe referencing an anchor that no `anchor`
        // statement declared — the wiring still resolves to entities.
        for (side, id, anchor_name, span) in [
            ("from", from_id, &pipe.from.anchor, pipe.from.span),
            ("to", to_id, &pipe.to.anchor, pipe.to.span),
        ] {
            let known = anchors_by_entity
                .get(&id)
                .map(|s| s.contains(anchor_name))
                .unwrap_or(false);
            if !known {
                diagnostics.push(Diagnostic::warning(
                    format!("pipe {side} anchor `{anchor_name}` was not declared with an `anchor` statement"),
                    span,
                ));
            }
        }

        opinions.push(Opinion::Edge(Edge {
            id: EdgeId::new(
                (from_id, &pipe.from.anchor),
                (to_id, &pipe.to.anchor),
            ),
            from: (from_id, pipe.from.anchor.clone()),
            to: (to_id, pipe.to.anchor.clone()),
        }));
    }
}

fn resolve_endpoint(
    selector: &str,
    lsg: &Lsg,
    resolved: &HashMap<String, EntityId>,
) -> Option<EntityId> {
    if let Some(id) = resolved.get(selector) {
        return Some(*id);
    }
    lsg.resolve_selector(selector).map(|e| e.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsg::{Aabb, Entity, Transform};

    fn ent(path: &str, tag: &str) -> Entity {
        let mut vf = std::collections::HashMap::new();
        vf.insert("assetTag".to_string(), serde_json::Value::String(tag.to_string()));
        Entity {
            id: EntityId::from_prim_path(path),
            prim_path: path.to_string(),
            parent: None,
            children: vec![],
            kind: Some("component".to_string()),
            tags: vec![],
            vf,
            transform_default: Transform::identity(),
            extents: Aabb::zero(),
            geom_ref: None,
            bindings: vec![],
        }
    }

    fn scene_lsg() -> Lsg {
        let mut lsg = Lsg::new();
        lsg.insert(ent("/PS/Pump_01", "PUMP-01"));
        lsg.insert(ent("/PS/Tank_A", "TANK-A"));
        lsg.link_hierarchy();
        lsg
    }

    #[test]
    fn lowers_parts_and_pipes_to_opinions() {
        let lsg = scene_lsg();
        let src = r#"
scene "PS"
part PUMP-01 {
  tag "duty"
  anchor discharge at (0.9, 0, 0)
  bind flow metric("pump_flow_gpm{asset=\"PUMP-01\"}") unit "gpm"
}
part TANK-A {
  anchor inlet at (0, 0, 1)
}
pipe PUMP-01.discharge -> TANK-A.inlet
"#;
        let r = compile(src, &lsg);
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        // 1 tag + 1 anchor + 1 binding + 1 anchor + 1 edge = 5 opinions.
        assert_eq!(r.opinions.len(), 5);
        assert!(r.opinions.iter().any(|o| matches!(o, Opinion::Edge(_))));
    }

    #[test]
    fn unknown_part_reports_at_span() {
        let lsg = scene_lsg();
        let src = "scene \"PS\"\npart NOPE {\n  tag \"x\"\n}\n";
        let r = compile(src, &lsg);
        assert!(r.has_errors());
        let e = r.diagnostics.iter().find(|d| d.is_error()).unwrap();
        assert!(e.message.contains("unknown part `NOPE`"));
        assert_eq!(e.span.line, 2);
    }

    #[test]
    fn dangling_pipe_anchor_warns_not_errors() {
        let lsg = scene_lsg();
        let src = "scene \"PS\"\npipe PUMP-01.x -> TANK-A.y\n";
        let r = compile(src, &lsg);
        assert!(!r.has_errors(), "diags: {:?}", r.diagnostics);
        assert!(r.diagnostics.iter().any(|d| !d.is_error()));
    }
}
