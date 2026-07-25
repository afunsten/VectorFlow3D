//! VectorFlow3D Scene Graph Service — Phase 1 CLI.
//!
//! Indexes an OpenUSD root layer into the Logical Scene Graph (payloads
//! unloaded) and manages committed pins in the SQLite Twin Overlay. No Interest
//! Manager / Runtime Scene Graph / telemetry resolvers / renderer bridge yet.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand};

use sha2::{Digest, Sha256};

use vectorflow_sgs::alert::{force_subscription, AlertSource, StubAlertSource};
use vectorflow_sgs::bridge::BridgeServer;
use vectorflow_sgs::dsl;
use vectorflow_sgs::fake_bridge::FakeBridge;
use vectorflow_sgs::hydrate::{PayloadCache, PayloadLoader, StubPayloadLoader, UsdPayloadLoader};
use vectorflow_sgs::import;
use vectorflow_sgs::interest::{InterestManager, Region, Subscription};
use vectorflow_sgs::lsg::{Entity, EntityId, Lsg, Transform};
use vectorflow_sgs::opinion;
use vectorflow_sgs::overlay::TwinOverlay;
use vectorflow_sgs::resolver::{resolve_active, Resolver, StubResolver, VictoriaMetricsResolver};
use vectorflow_sgs::rsg::Rsg;
use vectorflow_sgs::spatial::SpatialIndex;
use vectorflow_sgs::synth;

#[derive(Parser)]
#[command(
    name = "sgs",
    about = "VectorFlow3D Scene Graph Service (Phase 1: LSG index + Twin Overlay)",
    version
)]
struct Cli {
    /// Twin Overlay SQLite database path.
    #[arg(long, global = true, env = "VF_OVERLAY_DB", default_value = "vf-twin-overlay.sqlite")]
    overlay: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Import a USD root layer (or NDJSON dump) into the LSG and print stats.
    Import {
        #[command(flatten)]
        source: SourceArgs,
    },
    /// Build a synthetic ~10M-ref LSG and report cold build time + RSS.
    Synth {
        #[arg(long, default_value_t = 10_000_000)]
        count: usize,
    },
    /// Pin a transform override for an entity (Twin Overlay).
    Pin {
        /// Asset tag (e.g. PUMP-01) or prim path (e.g. /PumpStation01/PumpHall/Pump_01).
        selector: String,
        /// Translation "x,y,z".
        #[arg(long)]
        translate: String,
        /// Who authored the pin.
        #[arg(long)]
        by: Option<String>,
        #[command(flatten)]
        source: SourceArgs,
    },
    /// Remove a pin.
    Unpin {
        selector: String,
        #[command(flatten)]
        source: SourceArgs,
    },
    /// Show an entity's authored default vs resolved (pinned) transform.
    Show {
        selector: String,
        #[command(flatten)]
        source: SourceArgs,
    },
    /// Phase 2: drive a moving-AOI + selection interest demo over the RSG.
    Interest {
        #[command(flatten)]
        source: SourceArgs,
        /// Use a synthetic world of N entities instead of a USD/NDJSON source.
        #[arg(long)]
        synth: Option<usize>,
        /// AOI sphere center "x,y,z" at step 0.
        #[arg(long, default_value = "0,0,0")]
        aoi_center: String,
        /// AOI sphere radius (world units).
        #[arg(long, default_value_t = 5.0)]
        aoi_radius: f64,
        /// Explicit selection: comma-separated asset tags / prim paths, kept
        /// active regardless of the AOI (spec §3.2).
        #[arg(long)]
        select: Option<String>,
        /// Max entities the subscription may activate (spec §3.2 budget).
        #[arg(long, default_value_t = 5000)]
        budget: usize,
        /// Number of AOI steps to simulate.
        #[arg(long, default_value_t = 5)]
        steps: usize,
        /// Per-step AOI translation "x,y,z".
        #[arg(long, default_value = "5,0,0")]
        step_delta: String,
        /// Ticks an unreferenced entity waits before eviction (grace period).
        #[arg(long, default_value_t = 2)]
        grace_steps: u64,
        /// Optional spatial-index cell size override.
        #[arg(long)]
        cell_size: Option<f64>,
    },
    /// Phase 3: resolve telemetry lazily for the active RSG working set.
    Resolve {
        #[command(flatten)]
        source: SourceArgs,
        /// Use a synthetic world of N entities instead of a USD/NDJSON source.
        #[arg(long)]
        synth: Option<usize>,
        /// AOI sphere center "x,y,z" at step 0.
        #[arg(long, default_value = "0,0,0")]
        aoi_center: String,
        /// AOI sphere radius (world units).
        #[arg(long, default_value_t = 5.0)]
        aoi_radius: f64,
        /// Explicit selection kept active regardless of the AOI.
        #[arg(long)]
        select: Option<String>,
        /// Max entities the subscription may activate (spec §3.2 budget).
        #[arg(long, default_value_t = 5000)]
        budget: usize,
        /// Number of AOI steps to simulate.
        #[arg(long, default_value_t = 4)]
        steps: usize,
        /// Per-step AOI translation "x,y,z".
        #[arg(long, default_value = "5,0,0")]
        step_delta: String,
        /// Ticks an unreferenced entity waits before eviction (grace period).
        #[arg(long, default_value_t = 2)]
        grace_steps: u64,
        /// VictoriaMetrics base URL (PromQL endpoint).
        #[arg(long, env = "VF_VM_URL", default_value = "http://127.0.0.1:8428")]
        vm_url: String,
        /// Resolve against an in-process stub instead of VictoriaMetrics (no
        /// network; for CI / demos without a running VM).
        #[arg(long)]
        offline: bool,
        /// Value the offline stub resolver returns for every binding.
        #[arg(long, default_value_t = 42.0)]
        stub_value: f64,
        /// Simulate a resolver outage (offline stub returns no data) to exercise
        /// the stale-while-revalidate fallback.
        #[arg(long)]
        outage: bool,
        /// Inject a stub alert for this selector (asset tag / prim path),
        /// force-activating it regardless of the AOI (spec §3.2).
        #[arg(long)]
        alert: Option<String>,
        /// Step at which to inject the alert.
        #[arg(long, default_value_t = 1)]
        alert_step: usize,
    },
    /// Phase 5: drive a fake renderer bridge over the `vf.bridge.v1` snapshot +
    /// diff protocol, then prove pin write-back, coarse pick, and reconnect
    /// resync — no engine required.
    Bridge {
        #[command(flatten)]
        source: SourceArgs,
        /// Use a synthetic world of N entities instead of a USD/NDJSON source.
        #[arg(long)]
        synth: Option<usize>,
        /// AOI sphere center "x,y,z" at step 0.
        #[arg(long, default_value = "0,0,0")]
        aoi_center: String,
        /// AOI sphere radius (world units).
        #[arg(long, default_value_t = 5.0)]
        aoi_radius: f64,
        /// Explicit selection kept active regardless of the AOI.
        #[arg(long)]
        select: Option<String>,
        /// Max entities the subscription may activate (spec §3.2 budget).
        #[arg(long, default_value_t = 5000)]
        budget: usize,
        /// Number of AOI steps to simulate.
        #[arg(long, default_value_t = 4)]
        steps: usize,
        /// Per-step AOI translation "x,y,z".
        #[arg(long, default_value = "5,0,0")]
        step_delta: String,
        /// Ticks an unreferenced entity waits before eviction (grace period).
        #[arg(long, default_value_t = 2)]
        grace_steps: u64,
        /// Selector to pin mid-run (asset tag / prim path) to demo write-back.
        #[arg(long)]
        pin: Option<String>,
        /// Translation "x,y,z" for the `--pin` write-back.
        #[arg(long, default_value = "0,0,10")]
        pin_translate: String,
    },
    /// Phase 5.5: serve the `vf.bridge.v1` stream over a real WebSocket for the
    /// observer WebGPU client (blocking, no tokio; one Subscription/connection).
    Serve {
        #[command(flatten)]
        source: SourceArgs,
        /// Use a synthetic world of N entities instead of a USD/NDJSON source.
        #[arg(long)]
        synth: Option<usize>,
        /// Address to bind the WebSocket listener on.
        #[arg(long, default_value = "127.0.0.1:8787")]
        addr: String,
        /// Default AOI sphere radius (world units) for each connection.
        #[arg(long, default_value_t = 8.0)]
        aoi_radius: f64,
        /// Max entities a connection's subscription may activate (spec §3.2).
        #[arg(long, default_value_t = 5000)]
        budget: usize,
        /// Ticks an unreferenced entity waits before eviction (grace period).
        #[arg(long, default_value_t = 2)]
        grace_steps: u64,
        /// Tint boxes from a live VictoriaMetrics instead of the offline stub.
        /// Off by default so the page renders with zero external infra.
        #[arg(long)]
        vm: bool,
        /// VictoriaMetrics base URL (PromQL endpoint) used when `--vm` is set.
        #[arg(long, env = "VF_VM_URL", default_value = "http://127.0.0.1:8428")]
        vm_url: String,
        /// Value the offline stub resolver returns for every binding.
        #[arg(long, default_value_t = 42.0)]
        stub_value: f64,
        /// Simulate a resolver outage (offline stub returns no data) so the
        /// client shows `stale`/`unavailable` tints.
        #[arg(long)]
        outage: bool,
    },
    /// Phase 4: compile a Flow3D DSL file into Twin-Overlay opinions and patch
    /// the LSG. `--reload <edited.flow3d>` proves an incremental reload does not
    /// storm the RSG.
    Compile {
        /// The `.flow3d` source file to compile.
        flow: PathBuf,
        #[command(flatten)]
        source: SourceArgs,
        /// Use a synthetic world of N entities instead of a USD/NDJSON source.
        #[arg(long)]
        synth: Option<usize>,
        /// An edited version of the DSL to reload against the same overlay key,
        /// demonstrating the minimal patch + no-RSG-storm re-evaluation.
        #[arg(long)]
        reload: Option<PathBuf>,
        /// AOI sphere center "x,y,z" for the reload storm check.
        #[arg(long, default_value = "0,3,0")]
        aoi_center: String,
        /// AOI sphere radius for the reload storm check.
        #[arg(long, default_value_t = 50.0)]
        aoi_radius: f64,
    },
}

/// Where to load the LSG from. Shared by commands that resolve selectors.
#[derive(Args, Clone)]
struct SourceArgs {
    /// USD root layer to compose (payloads unloaded) via the Python helper.
    usd_root: Option<PathBuf>,
    /// Ingest a pre-exported NDJSON dump instead of running the helper.
    #[arg(long)]
    from_json: Option<PathBuf>,
    /// Python interpreter for the USD helper (default: tools/.venv then python3).
    #[arg(long)]
    python: Option<String>,
}

impl SourceArgs {
    /// Build the LSG if a source was provided.
    fn load(&self) -> Result<Option<Lsg>> {
        if let Some(json) = &self.from_json {
            let f = std::fs::File::open(json)
                .with_context(|| format!("opening NDJSON dump {}", json.display()))?;
            return Ok(Some(import::build_from_ndjson(f)?));
        }
        if let Some(root) = &self.usd_root {
            return Ok(Some(import::import_from_usd(root, self.python.as_deref())?));
        }
        Ok(None)
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Import { source } => cmd_import(&source),
        Command::Synth { count } => cmd_synth(count),
        Command::Pin {
            selector,
            translate,
            by,
            source,
        } => cmd_pin(&cli.overlay, &selector, &translate, by.as_deref(), &source),
        Command::Unpin { selector, source } => cmd_unpin(&cli.overlay, &selector, &source),
        Command::Show { selector, source } => cmd_show(&cli.overlay, &selector, &source),
        Command::Interest {
            source,
            synth,
            aoi_center,
            aoi_radius,
            select,
            budget,
            steps,
            step_delta,
            grace_steps,
            cell_size,
        } => cmd_interest(InterestArgs {
            source,
            synth,
            aoi_center,
            aoi_radius,
            select,
            budget,
            steps,
            step_delta,
            grace_steps,
            cell_size,
        }),
        Command::Resolve {
            source,
            synth,
            aoi_center,
            aoi_radius,
            select,
            budget,
            steps,
            step_delta,
            grace_steps,
            vm_url,
            offline,
            stub_value,
            outage,
            alert,
            alert_step,
        } => cmd_resolve(ResolveArgs {
            source,
            synth,
            aoi_center,
            aoi_radius,
            select,
            budget,
            steps,
            step_delta,
            grace_steps,
            vm_url,
            offline,
            stub_value,
            outage,
            alert,
            alert_step,
        }),
        Command::Bridge {
            source,
            synth,
            aoi_center,
            aoi_radius,
            select,
            budget,
            steps,
            step_delta,
            grace_steps,
            pin,
            pin_translate,
        } => cmd_bridge(BridgeArgs {
            overlay: cli.overlay,
            source,
            synth,
            aoi_center,
            aoi_radius,
            select,
            budget,
            steps,
            step_delta,
            grace_steps,
            pin,
            pin_translate,
        }),
        Command::Serve {
            source,
            synth,
            addr,
            aoi_radius,
            budget,
            grace_steps,
            vm,
            vm_url,
            stub_value,
            outage,
        } => cmd_serve(ServeArgs {
            overlay: cli.overlay,
            source,
            synth,
            addr,
            aoi_radius,
            budget,
            grace_steps,
            vm,
            vm_url,
            stub_value,
            outage,
        }),
        Command::Compile {
            flow,
            source,
            synth,
            reload,
            aoi_center,
            aoi_radius,
        } => cmd_compile(CompileArgs {
            overlay: cli.overlay,
            flow,
            source,
            synth,
            reload,
            aoi_center,
            aoi_radius,
        }),
    }
}

fn cmd_import(source: &SourceArgs) -> Result<()> {
    if source.usd_root.is_none() && source.from_json.is_none() {
        bail!("provide a USD root layer path or --from-json <dump>");
    }
    let started = Instant::now();
    let lsg = source
        .load()?
        .expect("source presence checked above");
    let elapsed = started.elapsed();

    if lsg.is_empty() {
        bail!("no prims were indexed from the source");
    }

    println!("LSG index built in {:.2?}", elapsed);
    println!("  entities:              {}", lsg.len());
    println!("  index revision:        {}", lsg.revision());
    println!("  payload refs (unloaded): {}", lsg.payload_count());
    println!("  entities with bindings: {}", lsg.entities_with_bindings());
    println!("  binding descriptors:    {}", lsg.binding_count());

    // Kind breakdown.
    let mut assemblies = 0;
    let mut groups = 0;
    let mut components = 0;
    for e in lsg.entities() {
        match e.kind.as_deref() {
            Some("assembly") => assemblies += 1,
            Some("group") => groups += 1,
            Some("component") => components += 1,
            _ => {}
        }
    }
    println!(
        "  kinds: {assemblies} assembly / {groups} group / {components} component"
    );

    // Binding index summary (distinct attributes -> entity count via the index).
    let mut attrs: Vec<String> = lsg
        .entities()
        .flat_map(|e| e.bindings.iter().map(|b| b.attribute.clone()))
        .collect();
    attrs.sort();
    attrs.dedup();
    if !attrs.is_empty() {
        let summary: Vec<String> = attrs
            .iter()
            .map(|a| format!("{a}={}", lsg.entities_binding(a).len()))
            .collect();
        println!("  binding index ({} attrs): {}", attrs.len(), summary.join(" "));
    }

    // Asset tags (sorted) with their bound attributes — proves instance-level
    // data was indexed without opening payloads.
    let mut tagged: Vec<&Entity> = lsg
        .entities()
        .filter(|e| e.asset_tag().is_some())
        .collect();
    tagged.sort_by_key(|e| e.asset_tag().unwrap().to_string());
    if !tagged.is_empty() {
        println!("\nindexed assets (payloads NOT loaded):");
        for e in tagged {
            let attrs: Vec<&str> = e.bindings.iter().map(|b| b.attribute.as_str()).collect();
            let payload = e
                .geom_ref
                .as_ref()
                .map(|g| g.payload_uri.as_str())
                .unwrap_or("-");
            println!(
                "  {:8} {:<10} pos={:?} payload={} bindings=[{}]",
                e.asset_tag().unwrap(),
                e.kind.as_deref().unwrap_or("?"),
                e.transform_default.translation(),
                payload,
                attrs.join(", ")
            );
        }
    }
    Ok(())
}

fn cmd_synth(count: usize) -> Result<()> {
    println!("generating synthetic LSG of {count} entities (payloads unloaded)...");
    let started = Instant::now();
    let lsg = synth::generate(count);
    let elapsed = started.elapsed();

    println!("cold build:   {:.2?}", elapsed);
    println!("entities:     {}", lsg.len());
    println!("payload refs: {}", lsg.payload_count());
    println!("bindings:     {}", lsg.binding_count());
    match synth::max_rss_bytes() {
        Some(rss) => println!("peak RSS:     {}", synth::format_bytes(rss)),
        None => println!("peak RSS:     (unavailable)"),
    }
    let gate = std::time::Duration::from_secs(30);
    println!(
        "gate (<30s cold build): {}",
        if elapsed < gate { "PASS" } else { "FAIL" }
    );
    Ok(())
}

/// Resolve a selector to `(EntityId, prim_path, authored_default)`. Uses the LSG
/// when a source is provided; otherwise a prim-path selector is hashed directly.
fn resolve_target(
    selector: &str,
    lsg: Option<&Lsg>,
) -> Result<(EntityId, String, Option<Transform>)> {
    if let Some(lsg) = lsg {
        match lsg.resolve_selector(selector) {
            Some(e) => Ok((e.id, e.prim_path.clone(), Some(e.transform_default))),
            None => bail!("selector '{selector}' not found in the LSG"),
        }
    } else if selector.starts_with('/') {
        Ok((EntityId::from_prim_path(selector), selector.to_string(), None))
    } else {
        bail!(
            "asset-tag selector '{selector}' needs a source to resolve; \
             pass a USD root / --from-json, or use a full prim path"
        )
    }
}

fn parse_translate(s: &str) -> Result<[f64; 3]> {
    let parts: Vec<&str> = s.split(',').map(str::trim).collect();
    if parts.len() != 3 {
        bail!("--translate expects 'x,y,z', got '{s}'");
    }
    let mut out = [0.0; 3];
    for (i, p) in parts.iter().enumerate() {
        out[i] = p
            .parse()
            .with_context(|| format!("parsing translate component '{p}'"))?;
    }
    Ok(out)
}

fn cmd_pin(
    overlay_path: &std::path::Path,
    selector: &str,
    translate: &str,
    by: Option<&str>,
    source: &SourceArgs,
) -> Result<()> {
    let lsg = source.load()?;
    let (id, prim_path, _default) = resolve_target(selector, lsg.as_ref())?;
    let t = Transform::from_translation(parse_translate(translate)?);

    let mut ov = TwinOverlay::open(overlay_path)?;
    let revision = ov.pin(id, &prim_path, t, by)?;
    println!(
        "pinned {} ({}) -> translate {:?}  [overlay revision {}]",
        selector,
        id,
        t.translation(),
        revision
    );
    Ok(())
}

fn cmd_unpin(
    overlay_path: &std::path::Path,
    selector: &str,
    source: &SourceArgs,
) -> Result<()> {
    let lsg = source.load()?;
    let (id, _prim_path, _default) = resolve_target(selector, lsg.as_ref())?;
    let mut ov = TwinOverlay::open(overlay_path)?;
    let (revision, existed) = ov.unpin(id)?;
    if existed {
        println!("unpinned {} ({})  [overlay revision {}]", selector, id, revision);
    } else {
        println!("no pin existed for {} ({})", selector, id);
    }
    Ok(())
}

fn cmd_show(
    overlay_path: &std::path::Path,
    selector: &str,
    source: &SourceArgs,
) -> Result<()> {
    let lsg = source.load()?;
    let (id, prim_path, default) = resolve_target(selector, lsg.as_ref())?;
    let ov = TwinOverlay::open(overlay_path)?;
    let pin = ov.get_pin(id)?;

    println!("entity:    {}", prim_path);
    println!("EntityId:  {}", id);
    let mut entity_ref = None;
    if let Some(lsg) = &lsg {
        if let Some(e) = lsg.get(id) {
            println!("kind:      {}", e.kind.as_deref().unwrap_or("-"));
            if let Some(cls) = e.class() {
                println!("class:     {cls}");
            }
            if let Some(tag) = e.asset_tag() {
                println!("assetTag:  {tag}");
            }
            if !e.tags.is_empty() {
                println!("tags:      {}", e.tags.join(", "));
            }
            if let Some(g) = &e.geom_ref {
                println!("geomRef:   {} {} (payload NOT loaded)", g.payload_uri, g.prim_path);
            }
            if !e.bindings.is_empty() {
                let attrs: Vec<&str> = e.bindings.iter().map(|b| b.attribute.as_str()).collect();
                println!("bindings:  {}", attrs.join(", "));
            }
            entity_ref = Some(e);
        }
    }
    match default {
        Some(d) => println!("authored:  translate {:?}", d.translation()),
        None => println!("authored:  (no source; unknown)"),
    }
    match &pin {
        Some(p) => println!(
            "pinned:    translate {:?}  by {}  at {}ms  [revision {}]",
            p.transform.translation(),
            p.pinned_by.as_deref().unwrap_or("?"),
            p.at_ms,
            p.revision
        ),
        None => println!("pinned:    (none)"),
    }
    // Resolved transform: prefer the overlay's own pin>authored resolution when
    // the entity is in the LSG; otherwise fall back to whatever we know.
    let resolved = match (entity_ref, &pin, default) {
        (Some(e), _, _) => Some(ov.resolved_transform(e)?),
        (None, Some(p), _) => Some(p.transform),
        (None, None, Some(d)) => Some(d),
        (None, None, None) => None,
    };
    match resolved {
        Some(r) => println!("resolved:  translate {:?}  (pin > authored)", r.translation()),
        None => println!("resolved:  (unknown without a source)"),
    }
    println!(
        "overlay:   {} pin(s), revision {}",
        ov.pin_count()?,
        ov.revision()?
    );
    Ok(())
}

struct InterestArgs {
    source: SourceArgs,
    synth: Option<usize>,
    aoi_center: String,
    aoi_radius: f64,
    select: Option<String>,
    budget: usize,
    steps: usize,
    step_delta: String,
    grace_steps: u64,
    cell_size: Option<f64>,
}

/// Load the LSG and pick a payload loader. Synthetic / NDJSON worlds have no
/// on-disk component assets, so they hydrate with the stub loader; a real USD
/// root resolves payloads next to the root via the Python helper. Shared by the
/// `interest` and `resolve` demos.
fn load_world(
    source: &SourceArgs,
    synth: Option<usize>,
) -> Result<(Lsg, Box<dyn PayloadLoader>, String)> {
    if let Some(n) = synth {
        Ok((
            synth::generate(n),
            Box::new(StubPayloadLoader),
            format!("synthetic ({n} entities, stub hydration)"),
        ))
    } else if let Some(root) = &source.usd_root {
        let base = root
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let lsg = import::import_from_usd(root, source.python.as_deref())?;
        Ok((
            lsg,
            Box::new(UsdPayloadLoader::new(base, source.python.clone())),
            format!("USD root {} (payload hydration via helper)", root.display()),
        ))
    } else if let Some(json) = &source.from_json {
        let f = std::fs::File::open(json)
            .with_context(|| format!("opening NDJSON dump {}", json.display()))?;
        Ok((
            import::build_from_ndjson(f)?,
            Box::new(StubPayloadLoader),
            format!("NDJSON {} (stub hydration)", json.display()),
        ))
    } else {
        bail!("provide a USD root layer, --from-json <dump>, or --synth <count>");
    }
}

fn cmd_interest(args: InterestArgs) -> Result<()> {
    let (lsg, loader, source_desc) = load_world(&args.source, args.synth)?;

    if lsg.is_empty() {
        bail!("no entities were indexed from the source");
    }
    let start_revision = lsg.revision();

    let idx = match args.cell_size {
        Some(cs) => SpatialIndex::build_with_cell_size(&lsg, cs),
        None => SpatialIndex::build(&lsg),
    };

    let mut center = parse_translate(&args.aoi_center)?;
    let delta = parse_translate(&args.step_delta)?;

    // Resolve explicit selection (kept active regardless of the AOI).
    let mut selection = Vec::new();
    if let Some(sel) = &args.select {
        for s in sel.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match lsg.resolve_selector(s) {
                Some(e) => selection.push(e.id),
                None => eprintln!("warning: selection '{s}' not found in the LSG"),
            }
        }
    }

    let mut sub = Subscription::spatial(
        1,
        Region::Sphere {
            center,
            radius: args.aoi_radius,
        },
        args.budget,
    );
    sub.entity_ids = selection.clone();

    let mut im = InterestManager::new();
    im.upsert(sub).map_err(|e| anyhow!(e))?;
    let mut rsg = Rsg::new(args.grace_steps);
    let mut cache = PayloadCache::new(loader);

    println!("source:       {source_desc}");
    println!("LSG entities: {}", lsg.len());
    println!(
        "spatial index: {} occupied cells @ cell_size {}",
        idx.cell_count(),
        idx.cell_size()
    );
    println!(
        "subscription: sphere r={} budget={} selection={} grace={} steps\n",
        args.aoi_radius,
        args.budget,
        selection.len(),
        args.grace_steps
    );

    for step in 0..args.steps {
        if step > 0 {
            center = [
                center[0] + delta[0],
                center[1] + delta[1],
                center[2] + delta[2],
            ];
            im.set_region(
                1,
                Region::Sphere {
                    center,
                    radius: args.aoi_radius,
                },
            );
        }
        let now = step as u64;
        let eval_started = Instant::now();
        let t = im.evaluate(&lsg, &idx);
        let eval_elapsed = eval_started.elapsed();
        rsg.apply(&t, &lsg, &mut cache, now)?;
        let evicted = rsg.evict_expired(now, &mut cache);
        let diff = rsg.take_diff(1);
        let active = im.active_count(1);

        anyhow::ensure!(
            active <= args.budget,
            "budget violated at step {step}: active {active} > budget {}",
            args.budget
        );

        println!(
            "step {step}: center=[{:.0},{:.0},{:.0}]  eval={:.2?}  +{}/-{}  |RSG|={}  open_payloads={}  pending_evict={}  evicted={}  diff(+{}/-{})",
            center[0],
            center[1],
            center[2],
            eval_elapsed,
            t.activated.len(),
            t.deactivated.len(),
            rsg.len(),
            cache.loaded_count(),
            rsg.pending_eviction_count(),
            evicted.len(),
            diff.upserts.len(),
            diff.removes.len(),
        );
    }

    // Surface a sample of the deferred component metadata revealed by loading a
    // payload (invisible to the Phase 1 index pass).
    if let Some(re) = rsg
        .entities()
        .filter(|re| re.hydrated.is_some())
        .min_by_key(|re| re.id)
    {
        if let Some(h) = &re.hydrated {
            let bbox = h
                .bbox
                .map(|b| format!("{:?}..{:?}", b.min, b.max))
                .unwrap_or_else(|| "(none)".to_string());
            println!(
                "\nhydrated sample: {} kind={} class={} bbox={} geomPrims={}",
                re.prim_path,
                h.kind.as_deref().unwrap_or("-"),
                h.class.as_deref().unwrap_or("-"),
                bbox,
                h.prim_count,
            );
        }
    }

    println!("\nbudget respected:      yes (|RSG| for sub 1 capped at {})", args.budget);
    println!("distinct payloads open: {}", cache.loaded_count());
    println!(
        "payload load calls:     {} (cache misses; identical payloads hydrate once)",
        cache.load_calls()
    );
    println!(
        "LSG revision:           {} -> {} (unchanged; interest never mutates the index)",
        start_revision,
        lsg.revision()
    );
    println!("USD writes:             0 (payload hydration is read-only; no stage mutation)");
    Ok(())
}

struct ResolveArgs {
    source: SourceArgs,
    synth: Option<usize>,
    aoi_center: String,
    aoi_radius: f64,
    select: Option<String>,
    budget: usize,
    steps: usize,
    step_delta: String,
    grace_steps: u64,
    vm_url: String,
    offline: bool,
    stub_value: f64,
    outage: bool,
    alert: Option<String>,
    alert_step: usize,
}

/// Wall-clock time in milliseconds since the epoch.
fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn cmd_resolve(args: ResolveArgs) -> Result<()> {
    let (lsg, loader, source_desc) = load_world(&args.source, args.synth)?;
    if lsg.is_empty() {
        bail!("no entities were indexed from the source");
    }
    let start_revision = lsg.revision();
    let idx = SpatialIndex::build(&lsg);

    let mut center = parse_translate(&args.aoi_center)?;
    let delta = parse_translate(&args.step_delta)?;

    // Explicit selection kept active regardless of the AOI.
    let mut selection = Vec::new();
    if let Some(sel) = &args.select {
        for s in sel.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match lsg.resolve_selector(s) {
                Some(e) => selection.push(e.id),
                None => eprintln!("warning: selection '{s}' not found in the LSG"),
            }
        }
    }

    let mut sub = Subscription::spatial(
        1,
        Region::Sphere {
            center,
            radius: args.aoi_radius,
        },
        args.budget,
    );
    sub.entity_ids = selection.clone();

    let mut im = InterestManager::new();
    im.upsert(sub).map_err(|e| anyhow!(e))?;
    let mut rsg = Rsg::new(args.grace_steps);
    let mut cache = PayloadCache::new(loader);

    // The resolver is the ONLY component that speaks PromQL (domain boundary).
    let mut resolver: Box<dyn Resolver> = if args.outage {
        Box::new(StubResolver::outage())
    } else if args.offline {
        Box::new(StubResolver::new(args.stub_value))
    } else {
        Box::new(VictoriaMetricsResolver::new(&args.vm_url))
    };
    let backend = if args.outage {
        "stub (simulated outage)".to_string()
    } else if args.offline {
        format!("stub (canned {})", args.stub_value)
    } else {
        format!("VictoriaMetrics {}", args.vm_url)
    };

    let mut alerts = StubAlertSource::new();

    println!("source:       {source_desc}");
    println!("LSG entities: {}", lsg.len());
    println!("resolver:     {backend}");
    println!(
        "subscription: sphere r={} budget={} selection={} grace={} steps\n",
        args.aoi_radius, args.budget, selection.len(), args.grace_steps
    );

    for step in 0..args.steps {
        if step > 0 {
            center = [center[0] + delta[0], center[1] + delta[1], center[2] + delta[2]];
            im.set_region(1, Region::Sphere { center, radius: args.aoi_radius });
        }

        // Alert push -> forced subscription (spec §3.2): the implicated prim is
        // force-activated regardless of the AOI.
        if let Some(sel) = &args.alert {
            if step == args.alert_step {
                alerts.push(sel.clone(), "cli_injected_alert");
                let events = alerts.drain();
                let forced = force_subscription(&events, &lsg, &mut im, 900, 64);
                println!(
                    "  ! alert: force-subscribed {} entity(ies) for selector '{}'",
                    forced.len(),
                    sel
                );
            }
        }

        let now = step as u64;
        let t = im.evaluate(&lsg, &idx);
        rsg.apply(&t, &lsg, &mut cache, now)?;
        rsg.evict_expired(now, &mut cache);

        let stats = resolve_active(&mut rsg, &lsg, resolver.as_mut(), now_ms());

        println!(
            "step {step}: center=[{:.0},{:.0},{:.0}]  |RSG|={}  issued={} hits={} (hit_ratio={:.0}%)  ok={} stale={} unavail={}  upstream(+{})",
            center[0],
            center[1],
            center[2],
            rsg.len(),
            stats.requests_issued,
            stats.cache_hits,
            stats.hit_ratio() * 100.0,
            stats.ok,
            stats.stale,
            stats.unavailable,
            stats.upstream_delta,
        );
    }

    // Sample a resolved entity's telemetry to show values land in the RSG only.
    if let Some(re) = rsg
        .entities()
        .filter(|re| !re.telemetry.is_empty())
        .min_by_key(|re| re.id)
    {
        let mut attrs: Vec<(&String, _)> = re.telemetry.iter().collect();
        attrs.sort_by_key(|(a, _)| (*a).clone());
        let sample: Vec<String> = attrs
            .iter()
            .map(|(a, v)| format!("{a}={:.2} ({})", v.value, v.quality.as_str()))
            .collect();
        println!("\nresolved sample: {}  {}", re.prim_path, sample.join(" "));
    }

    println!("\nresolver batch calls:   {}", resolver.batch_calls());
    println!(
        "resolver upstream reqs: {} (metric-batched; inactive bindings never resolve)",
        resolver.upstream_requests()
    );
    println!(
        "LSG revision:           {} -> {} (unchanged; resolution never mutates the index)",
        start_revision,
        lsg.revision()
    );
    println!("USD writes:             0 (telemetry lives in the RSG cache; never written to USD)");
    Ok(())
}

struct BridgeArgs {
    overlay: PathBuf,
    source: SourceArgs,
    synth: Option<usize>,
    aoi_center: String,
    aoi_radius: f64,
    select: Option<String>,
    budget: usize,
    steps: usize,
    step_delta: String,
    grace_steps: u64,
    pin: Option<String>,
    pin_translate: String,
}

/// Phase 5: drive a fake renderer bridge over `vf.bridge.v1`. Reconstructs a
/// Render Scene from the diff stream, then demos pin write-back, coarse pick,
/// and reconnect resync — no engine required, zero USD writes.
fn cmd_bridge(args: BridgeArgs) -> Result<()> {
    use vectorflow_sgs::bridge::{negotiate, BridgeRequest, PROTOCOL_VERSION};
    use vectorflow_sgs::interest::SubscriptionId;
    use vectorflow_sgs::rsg::RsgDiff;

    const SUB: SubscriptionId = 1;

    let (lsg, loader, source_desc) = load_world(&args.source, args.synth)?;
    if lsg.is_empty() {
        bail!("no entities were indexed from the source");
    }
    let start_revision = lsg.revision();
    let idx = SpatialIndex::build(&lsg);

    let mut center = parse_translate(&args.aoi_center)?;
    let delta = parse_translate(&args.step_delta)?;

    // Explicit selection kept active regardless of the AOI.
    let mut selection = Vec::new();
    if let Some(sel) = &args.select {
        for s in sel.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            match lsg.resolve_selector(s) {
                Some(e) => selection.push(e.id),
                None => eprintln!("warning: selection '{s}' not found in the LSG"),
            }
        }
    }

    // Resolve the pin target up front and keep it selected: an operator pins an
    // entity they have selected, so it stays in the active set (and thus in the
    // Render Scene and the reconnect snapshot).
    let pin_target = match &args.pin {
        Some(sel) => match lsg.resolve_selector(sel) {
            Some(e) => {
                if !selection.contains(&e.id) {
                    selection.push(e.id);
                }
                Some((e.id, e.prim_path.clone()))
            }
            None => {
                eprintln!("warning: --pin selector '{sel}' not found in the LSG");
                None
            }
        },
        None => None,
    };

    let mut sub = Subscription::spatial(
        SUB,
        Region::Sphere {
            center,
            radius: args.aoi_radius,
        },
        args.budget,
    );
    sub.entity_ids = selection.clone();

    let mut im = InterestManager::new();
    im.upsert(sub).map_err(|e| anyhow!(e))?;
    let mut rsg = Rsg::new(args.grace_steps);
    let mut cache = PayloadCache::new(loader);
    let mut overlay = TwinOverlay::open(&args.overlay)?;
    let mut server = BridgeServer::new();
    let mut bridge = FakeBridge::new();

    // Connect handshake (spec §5: bridges negotiate on connect).
    let agreed = negotiate(&[PROTOCOL_VERSION.to_string()]).unwrap_or("<none>");
    bridge.apply(&[server.hello(&lsg)]);

    println!("source:       {source_desc}");
    println!("LSG entities: {}", lsg.len());
    println!("protocol:     {agreed} (bridge Hello scene_revision={})", bridge.scene_revision);
    println!(
        "subscription: sphere r={} budget={} selection={} grace={} steps\n",
        args.aoi_radius, args.budget, selection.len(), args.grace_steps
    );

    // ---- live diff stream: the bridge reconstructs the scene from diffs ----
    let mut total_msgs = 0usize;
    for step in 0..args.steps {
        if step > 0 {
            center = [center[0] + delta[0], center[1] + delta[1], center[2] + delta[2]];
            im.set_region(SUB, Region::Sphere { center, radius: args.aoi_radius });
        }
        let now = step as u64;
        let t = im.evaluate(&lsg, &idx);
        rsg.apply(&t, &lsg, &mut cache, now)?;
        rsg.evict_expired(now, &mut cache);
        let diff = rsg.take_diff(SUB);
        let msgs = server.encode_diff(SUB, &diff, &lsg, &rsg, &overlay)?;
        bridge.apply(&msgs);
        let hydrated_now = bridge.hydrate(&mut cache)?;
        total_msgs += msgs.len();

        println!(
            "step {step}: center=[{:.0},{:.0},{:.0}]  |RSG|={}  render_scene={}  msgs={} (+{}/-{})  hydrated+{}",
            center[0],
            center[1],
            center[2],
            rsg.len(),
            bridge.len(),
            msgs.len(),
            diff.upserts.len(),
            diff.removes.len(),
            hydrated_now,
        );
    }

    // The Render Scene mirrors the subscriber's ACTIVE set, not the whole RSG:
    // shared pages linger in the RSG during their grace period after the
    // subscriber's diff already removed them from this view.
    let active = im.active_count(SUB);
    anyhow::ensure!(
        bridge.len() == active,
        "render scene ({}) must mirror the active set ({})",
        bridge.len(),
        active
    );
    println!("\nrender scene mirrors active set: yes ({} entities)", bridge.len());

    // ---- coarse pick (spec §1.3 coarse-pick non-goal) ----
    // Aim a ray through an active entity's centroid to guarantee a hit.
    if let Some(re) = rsg.entities().filter(|re| re.subscribers.contains(&SUB)).min_by_key(|re| re.id) {
        if let Some(e) = lsg.get(re.id) {
            let c = vectorflow_sgs::spatial::centroid(&e.extents);
            let origin = [c[0], c[1], c[2] - 1000.0];
            let dir = [0.0, 0.0, 1.0];
            let req = BridgeRequest::PickRequest { request_id: 1, origin, dir };
            // Server handles the request and replies to the bridge.
            if let BridgeRequest::PickRequest { request_id, origin, dir } = req {
                let result = server.coarse_pick(SUB, request_id, origin, dir, &lsg, &rsg);
                bridge.apply(&[result]);
            }
            match bridge.last_pick {
                Some(hit) => {
                    let tag = lsg.get(hit).and_then(|e| e.asset_tag()).unwrap_or("-");
                    println!("coarse pick: hit {} ({}) via ray-AABB", hit, tag);
                }
                None => println!("coarse pick: no hit"),
            }
        }
    }

    // ---- pin write-back (spec §3.5 PinPart -> Twin Overlay) ----
    if let Some((pin_id, prim_path)) = &pin_target {
        let (pin_id, prim_path) = (*pin_id, prim_path.as_str());
        let t = Transform::from_translation(parse_translate(&args.pin_translate)?);
        let confirm = server.handle_pin(pin_id, prim_path, t, Some("bridge_demo"), &mut overlay)?;
        bridge.apply(&[confirm.clone()]);
        let rev = match confirm {
            vectorflow_sgs::bridge::BridgeMsg::PinConfirm { revision, .. } => revision,
            _ => 0,
        };
        // The next upsert for this (active, selected) entity carries the pin.
        let redraw = RsgDiff { upserts: vec![pin_id], removes: vec![] };
        let msgs = server.encode_diff(SUB, &redraw, &lsg, &rsg, &overlay)?;
        bridge.apply(&msgs);
        let shown = bridge.get(pin_id).map(|re| re.transform.translation());
        println!(
            "pin write-back: {} -> {:?}  [overlay revision {}]  render scene shows {:?}",
            pin_id, t.translation(), rev, shown
        );
    }

    // ---- disconnect / reconnect resync (spec §3.5 snapshot + catch-up) ----
    // Capture the live scene, drop the bridge (disposable cache), reconnect and
    // rebuild purely from a fresh snapshot; the reconstruction must be identical.
    let before: Vec<(EntityId, _)> = bridge
        .entity_ids()
        .into_iter()
        .map(|id| (id, bridge.get(id).cloned()))
        .collect();

    bridge.disconnect();
    anyhow::ensure!(bridge.is_empty(), "disconnect must drop the disposable cache");
    bridge.apply(&[server.hello(&lsg)]);
    let snap = server.snapshot(SUB, &lsg, &rsg, &overlay)?;
    bridge.apply(&snap);
    bridge.hydrate(&mut cache)?;

    let after: Vec<(EntityId, _)> = bridge
        .entity_ids()
        .into_iter()
        .map(|id| (id, bridge.get(id).cloned()))
        .collect();
    let identical = before == after;
    println!(
        "\nreconnect: snapshot {} msgs, render scene {} -> {} entities  reconstructed identically: {}",
        snap.len(),
        before.len(),
        after.len(),
        if identical { "yes" } else { "NO — investigate" }
    );
    anyhow::ensure!(identical, "reconnect must reconstruct the scene identically");

    println!(
        "\nLSG revision:           {} -> {} (unchanged; the bridge never mutates the index)",
        start_revision,
        lsg.revision()
    );
    println!("total downstream msgs:  {}", total_msgs);
    println!("USD writes:             0 (bridge is a cache; pins persist in the Twin Overlay only)");
    Ok(())
}

struct ServeArgs {
    overlay: PathBuf,
    source: SourceArgs,
    synth: Option<usize>,
    addr: String,
    aoi_radius: f64,
    budget: usize,
    grace_steps: u64,
    vm: bool,
    vm_url: String,
    stub_value: f64,
    outage: bool,
}

/// Phase 5.5: serve `vf.bridge.v1` over a blocking WebSocket for the observer
/// WebGPU client. Reuses `load_world`'s branching to build the LSG + a
/// per-connection payload source, then runs the thread-per-connection server.
fn cmd_serve(args: ServeArgs) -> Result<()> {
    use vectorflow_sgs::geomstore::{GeomStore, MeshLoader, StubMeshLoader, UsdMeshLoader};
    use vectorflow_sgs::serve::{self, PayloadSource, ServeConfig, Shared, TelemetryConfig};

    // Build the LSG and decide how connections hydrate payloads. Mirrors
    // `load_world`, but returns a cloneable `PayloadSource` (a `Box<dyn
    // PayloadLoader>` cannot be shared across connection threads).
    let (lsg, payload, source_desc) = if let Some(n) = args.synth {
        (
            synth::generate(n),
            PayloadSource::Stub,
            format!("synthetic ({n} entities, stub hydration)"),
        )
    } else if let Some(root) = &args.source.usd_root {
        let base = root
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        let lsg = import::import_from_usd(root, args.source.python.as_deref())?;
        (
            lsg,
            PayloadSource::Usd {
                base,
                python: args.source.python.clone(),
            },
            format!("USD root {}", root.display()),
        )
    } else if let Some(json) = &args.source.from_json {
        let f = std::fs::File::open(json)
            .with_context(|| format!("opening NDJSON dump {}", json.display()))?;
        (
            import::build_from_ndjson(f)?,
            PayloadSource::Stub,
            format!("NDJSON {} (stub hydration)", json.display()),
        )
    } else {
        bail!("provide a USD root layer, --from-json <dump>, or --synth <count>");
    };

    if lsg.is_empty() {
        bail!("no entities were indexed from the source");
    }

    let telemetry = if args.outage {
        TelemetryConfig::Outage
    } else if args.vm {
        TelemetryConfig::Vm(args.vm_url)
    } else {
        TelemetryConfig::Offline(args.stub_value)
    };

    let index = SpatialIndex::build(&lsg);
    let overlay = TwinOverlay::open(&args.overlay)?;

    // VF geometry store (Phase 5.6): tessellate USD payloads via the helper for a
    // real USD source; synth/NDJSON worlds have no on-disk mesh assets (stub ->
    // no mesh -> the observer keeps its proxy box).
    let mesh_loader: Box<dyn MeshLoader> = match &payload {
        PayloadSource::Usd { base, python } => {
            Box::new(UsdMeshLoader::new(base.clone(), python.clone()))
        }
        PayloadSource::Stub => Box::new(StubMeshLoader),
    };
    let geom_store = GeomStore::from_lsg(mesh_loader, &lsg);

    println!("source:       {source_desc}");
    let shared = Shared {
        lsg: std::sync::Arc::new(lsg),
        index: std::sync::Arc::new(index),
        overlay: std::sync::Arc::new(std::sync::Mutex::new(overlay)),
        geom_store: std::sync::Arc::new(std::sync::Mutex::new(geom_store)),
        config: ServeConfig {
            aoi_radius: args.aoi_radius,
            budget: args.budget,
            grace_steps: args.grace_steps,
            telemetry,
            payload,
        },
    };
    serve::run(&args.addr, shared)
}

struct CompileArgs {
    overlay: PathBuf,
    flow: PathBuf,
    source: SourceArgs,
    synth: Option<usize>,
    reload: Option<PathBuf>,
    aoi_center: String,
    aoi_radius: f64,
}

fn sha256_hex(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    format!("{:x}", h.finalize())
}

/// Compile a Flow3D DSL file, render diagnostics, persist opinions to the Twin
/// Overlay, and patch the LSG (spec §3.9 / Phase 4).
fn cmd_compile(args: CompileArgs) -> Result<()> {
    let (mut lsg, loader, source_desc) = load_world(&args.source, args.synth)?;
    if lsg.is_empty() {
        bail!("no entities were indexed from the source (DSL selectors need a scene to resolve against)");
    }

    let flow_text = std::fs::read_to_string(&args.flow)
        .with_context(|| format!("reading DSL file {}", args.flow.display()))?;
    let flow_name = args.flow.display().to_string();

    let result = dsl::compile(&flow_text, &lsg);
    // Warnings first, then errors, both rendered with caret + line:col.
    let warnings: Vec<_> = result.diagnostics.iter().filter(|d| !d.is_error()).cloned().collect();
    if !warnings.is_empty() {
        eprintln!("{}\n", dsl::render_all(&warnings, &flow_text, &flow_name));
    }
    if result.has_errors() {
        let errors: Vec<_> = result.diagnostics.iter().filter(|d| d.is_error()).cloned().collect();
        eprintln!("{}", dsl::render_all(&errors, &flow_text, &flow_name));
        bail!("Flow3D compile failed: {} error(s)", result.error_count());
    }

    let source_key = flow_name.clone();
    let source_hash = sha256_hex(&flow_text);
    let mut ov = TwinOverlay::open(&args.overlay)?;
    let start_rev = lsg.revision();

    // Diff + persist (minimal patch) then reconcile the in-memory LSG.
    let prev = ov.load_opinions(&source_key)?;
    let (overlay_rev, diff) = ov.apply_opinions(&source_key, &source_hash, &result.opinions)?;
    let touched = opinion::reconcile(&mut lsg, &prev, &result.opinions);

    println!("source:        {source_desc}");
    println!("DSL file:      {flow_name}  (scene \"{}\")", result.scene_name);
    println!("LSG entities:  {}", lsg.len());
    println!(
        "opinions:      {} total  (+{} added / ~{} changed / -{} removed / {} unchanged)",
        result.opinions.len(),
        diff.added.len(),
        diff.changed.len(),
        diff.removed.len(),
        diff.unchanged.len(),
    );
    println!("touched entities: {}", touched.len());
    println!("anchors: {}   edges/pipes: {}", lsg.anchor_count(), lsg.edge_count());
    println!("LSG revision:  {} -> {}", start_rev, lsg.revision());
    println!("overlay revision: {overlay_rev}");
    println!("USD writes:    0 (opinions live in the Twin Overlay; vendor USD untouched)");

    if let Some(reload_path) = &args.reload {
        run_reload_demo(
            &mut lsg,
            loader,
            &mut ov,
            &source_key,
            &result.opinions,
            reload_path,
            &args.aoi_center,
            args.aoi_radius,
        )?;
    }

    Ok(())
}

/// Prove an incremental reload patches without storming the RSG: hold a camera
/// AOI's active set, apply an edited DSL as a minimal patch, then re-evaluate
/// the SAME Interest Manager and report the (near-zero) transitions.
#[allow(clippy::too_many_arguments)]
fn run_reload_demo(
    lsg: &mut Lsg,
    loader: Box<dyn PayloadLoader>,
    ov: &mut TwinOverlay,
    source_key: &str,
    prev_opinions: &[opinion::Opinion],
    reload_path: &std::path::Path,
    aoi_center: &str,
    aoi_radius: f64,
) -> Result<()> {
    println!("\n── reload demo (no-RSG-storm gate) ──");

    // Baseline: activate an AOI's working set before the reload.
    let center = parse_translate(aoi_center)?;
    let idx = SpatialIndex::build(lsg);
    let mut im = InterestManager::new();
    im.upsert(Subscription::spatial(
        1,
        Region::Sphere { center, radius: aoi_radius },
        1_000_000,
    ))
    .map_err(|e| anyhow!(e))?;
    let mut rsg = Rsg::new(2);
    let mut cache = PayloadCache::new(loader);
    let t0 = im.evaluate(lsg, &idx);
    rsg.apply(&t0, lsg, &mut cache, 0)?;
    let active_before = rsg.len();
    println!("baseline AOI: |RSG|={active_before} (activated {})", t0.activated.len());

    // Compile the edited DSL and apply it as a minimal patch.
    let text2 = std::fs::read_to_string(reload_path)
        .with_context(|| format!("reading reload DSL {}", reload_path.display()))?;
    let name2 = reload_path.display().to_string();
    let r2 = dsl::compile(&text2, lsg);
    if r2.has_errors() {
        let errors: Vec<_> = r2.diagnostics.iter().filter(|d| d.is_error()).cloned().collect();
        eprintln!("{}", dsl::render_all(&errors, &text2, &name2));
        bail!("reload compile failed: {} error(s)", r2.error_count());
    }
    let (_rev2, diff2) = ov.apply_opinions(source_key, &sha256_hex(&text2), &r2.opinions)?;
    let touched2 = opinion::reconcile(lsg, prev_opinions, &r2.opinions);

    // Re-evaluate the SAME interest manager over the patched LSG.
    let idx2 = SpatialIndex::build(lsg);
    let t1 = im.evaluate(lsg, &idx2);
    rsg.apply(&t1, lsg, &mut cache, 1)?;
    rsg.evict_expired(1, &mut cache);
    let active_after = rsg.len();

    println!(
        "reload patch: +{} / ~{} / -{}  touched entities={}",
        diff2.added.len(),
        diff2.changed.len(),
        diff2.removed.len(),
        touched2.len(),
    );
    println!(
        "interest re-eval: {} activations / {} deactivations  (|RSG| {} -> {})",
        t1.activated.len(),
        t1.deactivated.len(),
        active_before,
        active_after,
    );
    let stormed = t1.activated.len() + t1.deactivated.len() > touched2.len();
    println!(
        "no RSG storm:  {} (transitions ≤ touched entities; stable IDs, in-place patch)",
        if stormed { "NO — investigate" } else { "yes" }
    );
    Ok(())
}
