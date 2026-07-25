#!/usr/bin/env python3
"""VectorFlow3D USD -> LSG export helper (spec Phase 1).

Composes an OpenUSD root layer with **payloads unloaded**
(``Usd.Stage.Open(root, load=Usd.Stage.LoadNone)``) and streams one NDJSON
record per prim to stdout for the Rust SGS to ingest into the Logical Scene
Graph. Because payloads are never opened, only instance-level, index-friendly
data is emitted (spec 3.0): transform, ``extentsHint``, ``customData.vf``
identity/tags, and the declarative ``vf:binding:*`` descriptors, plus a
``geomRef`` pointing at the still-closed payload (with a content hash of the
referenced file).

Domain boundary: this reads only USD files. It never contacts VictoriaMetrics /
PromQL. Telemetry bindings are emitted as descriptors, not values.

Requires ``usd-core`` (see requirements.txt). Usage:

    usd_export.py <usd_root_layer>              # index pass (payloads UNLOADED)
    usd_export.py --payload <asset> [primPath]  # Phase 2 payload-load pass
    usd_export.py --mesh <asset> [primPath]     # Phase 5.6 tessellation pass

The ``--payload`` mode opens a single component payload WITH geometry loaded to
surface the component-internal defaults the index pass could not see (``kind``,
``customData.vf.class``, a coarse geometry bbox and prim count). It emits one
JSON object. It is read-only and never contacts VictoriaMetrics.

The ``--mesh`` mode (Phase 5.6) opens a single component payload and triangulates
its geometry into a single indexed mesh for the VF geometry store: it emits one
JSON object ``{"points":[[x,y,z],...],"normals":[...],"indices":[...]}`` with each
gprim's local transform baked in. It is deterministic (fixed tessellation),
read-only, and never contacts VictoriaMetrics.
"""

import hashlib
import json
import os
import sys

try:
    from pxr import Usd, UsdGeom, Sdf, Gf  # noqa: F401
except ImportError:
    sys.stderr.write(
        "error: could not import pxr (OpenUSD). Install usd-core:\n"
        "  python3 -m venv sgs/tools/.venv\n"
        "  sgs/tools/.venv/bin/pip install -r sgs/tools/requirements.txt\n"
    )
    sys.exit(2)


def sanitize(value):
    """Convert USD/Gf/Vt values into JSON-serializable Python primitives."""
    if isinstance(value, (str, bool, int, float)) or value is None:
        return value
    # Vt arrays, Gf vectors, and dict-like metadata are all iterable/mappable.
    if isinstance(value, dict):
        return {str(k): sanitize(v) for k, v in value.items()}
    # Gf.Vec* -> list of floats
    try:
        return [sanitize(v) for v in value]
    except TypeError:
        return str(value)


def read_vf_customdata(prim):
    """Composed ``customData.vf`` dictionary (may be empty when it lives behind
    an unloaded payload)."""
    vf = prim.GetCustomDataByKey("vf")
    if not vf:
        return {}
    return sanitize(dict(vf))


def read_extents_hint(prim):
    """Authored ``extentsHint`` as ``[[minx,miny,minz],[maxx,maxy,maxz]]`` or
    None."""
    attr = prim.GetAttribute("extentsHint")
    if not attr or not attr.HasAuthoredValue():
        return None
    val = attr.Get()
    if val is None or len(val) < 2:
        return None
    lo = [float(c) for c in val[0]]
    hi = [float(c) for c in val[1]]
    return [lo, hi]


def read_translate(prim):
    """Local translation from the prim's xform ops (translate-only in the
    bootstrap convention). Returns [x, y, z] or None."""
    xf = UsdGeom.Xformable(prim)
    if not xf:
        return None
    try:
        m = xf.GetLocalTransformation()  # Gf.Matrix4d, local-to-parent
    except Exception:
        return None
    t = m.ExtractTranslation()
    return [float(t[0]), float(t[1]), float(t[2])]


def read_payload(prim, root_dir):
    """Read the (unloaded) payload arc as a geomRef, without composing it."""
    if not prim.HasAuthoredPayloads():
        return None
    listop = prim.GetMetadata("payload")
    if listop is None:
        return None
    items = []
    for bucket in ("prependedItems", "appendedItems", "explicitItems"):
        items.extend(getattr(listop, bucket, []) or [])
    if not items:
        return None
    payload = items[0]
    asset_path = payload.assetPath
    prim_path = str(payload.primPath) if payload.primPath else ""

    content_hash = ""
    resolved = os.path.normpath(os.path.join(root_dir, asset_path))
    if os.path.isfile(resolved):
        h = hashlib.sha256()
        with open(resolved, "rb") as fh:
            for chunk in iter(lambda: fh.read(65536), b""):
                h.update(chunk)
        content_hash = h.hexdigest()

    return {
        "payloadUri": asset_path,
        "primPath": prim_path,
        "contentHash": content_hash,
    }


def read_bindings(prim):
    """Group ``vf:binding:<attr>:<field>`` attributes into binding descriptors."""
    grouped = {}
    for prop in prim.GetAuthoredProperties():
        name = prop.GetName()
        if not name.startswith("vf:binding:"):
            continue
        parts = name.split(":")
        # vf : binding : <attr> : <field>
        if len(parts) != 4:
            continue
        attr, field = parts[2], parts[3]
        value = prop.Get() if hasattr(prop, "Get") else None
        grouped.setdefault(attr, {})[field] = sanitize(value)

    bindings = []
    for attr, fields in sorted(grouped.items()):
        bindings.append(
            {
                "attribute": fields.get("attribute", attr),
                "sourceId": fields.get("sourceId", ""),
                "query": fields.get("query", ""),
                "unit": fields.get("unit", ""),
                "ttlMs": float(fields.get("ttlMs", 0) or 0),
                "priority": fields.get("priority", ""),
                "qualityPolicy": fields.get("qualityPolicy", ""),
            }
        )
    return bindings


def prim_kind(prim):
    """Authored kind; inferred as 'component' when the prim defers a payload
    (kind lives inside the unloaded payload, but a payload arc IS the component
    / streaming quantum per spec 3.2)."""
    kind = prim.GetMetadata("kind")
    if kind:
        return str(kind)
    if prim.HasAuthoredPayloads():
        return "component"
    return None


# ---- Phase 5.6: deterministic gprim tessellation -----------------------------
#
# The VF geometry store is glTF/GLB. OpenUSD stays import/asset-prep only, so
# tessellation happens here (out-of-process) rather than on the Rust build or the
# runtime hot path. Implicit gprims (Cube / Cylinder / Sphere) are tessellated
# with a fixed segment count so identical payloads produce byte-identical meshes;
# UsdGeomMesh prims are fan-triangulated. Each gprim's local transform (relative
# to the target component prim) is baked into the emitted vertices.

# Radial segments for implicit round gprims (Cylinder / Sphere). Fixed for
# determinism; identical payloads -> identical meshes -> one store entry.
_RADIAL_SEGMENTS = 24
_SPHERE_STACKS = 12


def _add_tri_soup(out_points, out_normals, out_indices, verts, normals, faces):
    """Append a list of local triangles (verts/normals + index triples) to the
    global buffers, offsetting the indices."""
    base = len(out_points)
    out_points.extend(verts)
    out_normals.extend(normals)
    for (a, b, c) in faces:
        out_indices.extend((base + a, base + b, base + c))


def _cube_local(size):
    """A cube of edge `size` centered at the origin, per-face normals (24 verts,
    12 tris) for flat shading."""
    h = size / 2.0
    faces_def = [
        ((0, 0, 1), [(-h, -h, h), (h, -h, h), (h, h, h), (-h, h, h)]),
        ((0, 0, -1), [(h, -h, -h), (-h, -h, -h), (-h, h, -h), (h, h, -h)]),
        ((1, 0, 0), [(h, -h, h), (h, -h, -h), (h, h, -h), (h, h, h)]),
        ((-1, 0, 0), [(-h, -h, -h), (-h, -h, h), (-h, h, h), (-h, h, -h)]),
        ((0, 1, 0), [(-h, h, h), (h, h, h), (h, h, -h), (-h, h, -h)]),
        ((0, -1, 0), [(-h, -h, -h), (h, -h, -h), (h, -h, h), (-h, -h, h)]),
    ]
    verts, normals, faces = [], [], []
    for n, quad in faces_def:
        b = len(verts)
        for v in quad:
            verts.append(v)
            normals.append(n)
        faces.append((b, b + 1, b + 2))
        faces.append((b, b + 2, b + 3))
    return verts, normals, faces


def _axis_frame(axis):
    """Return (axis_dir, u_dir, v_dir) unit vectors for a USD cylinder `axis`
    token so the circle is swept in the plane spanned by (u, v)."""
    if axis == "X":
        return (1.0, 0.0, 0.0), (0.0, 1.0, 0.0), (0.0, 0.0, 1.0)
    if axis == "Y":
        return (0.0, 1.0, 0.0), (0.0, 0.0, 1.0), (1.0, 0.0, 0.0)
    return (0.0, 0.0, 1.0), (1.0, 0.0, 0.0), (0.0, 1.0, 0.0)  # Z (default)


def _cylinder_local(height, radius, axis):
    """A cylinder of `height`/`radius` centered at the origin along `axis`, with a
    fixed radial segment count, side + two caps."""
    import math

    ad, ud, vd = _axis_frame(axis)
    hh = height / 2.0
    seg = _RADIAL_SEGMENTS
    verts, normals, faces = [], [], []

    def ring_point(theta, along):
        c, s = math.cos(theta), math.sin(theta)
        return (
            ad[0] * along + (ud[0] * c + vd[0] * s) * radius,
            ad[1] * along + (ud[1] * c + vd[1] * s) * radius,
            ad[2] * along + (ud[2] * c + vd[2] * s) * radius,
        )

    def radial_normal(theta):
        c, s = math.cos(theta), math.sin(theta)
        return (ud[0] * c + vd[0] * s, ud[1] * c + vd[1] * s, ud[2] * c + vd[2] * s)

    # Side wall (per-quad, radial normals).
    for i in range(seg):
        t0 = (i / seg) * 2.0 * math.pi
        t1 = ((i + 1) / seg) * 2.0 * math.pi
        n0, n1 = radial_normal(t0), radial_normal(t1)
        b = len(verts)
        verts.extend([ring_point(t0, hh), ring_point(t0, -hh), ring_point(t1, -hh), ring_point(t1, hh)])
        normals.extend([n0, n0, n1, n1])
        faces.append((b, b + 1, b + 2))
        faces.append((b, b + 2, b + 3))

    # End caps (fans around the axis centers).
    for along, cap_n, wind in ((hh, ad, False), (-hh, (-ad[0], -ad[1], -ad[2]), True)):
        center = (ad[0] * along, ad[1] * along, ad[2] * along)
        cb = len(verts)
        verts.append(center)
        normals.append(cap_n)
        rim = []
        for i in range(seg):
            t = (i / seg) * 2.0 * math.pi
            rim.append(len(verts))
            verts.append(ring_point(t, along))
            normals.append(cap_n)
        for i in range(seg):
            a = rim[i]
            b = rim[(i + 1) % seg]
            faces.append((cb, b, a) if wind else (cb, a, b))
    return verts, normals, faces


def _sphere_local(radius):
    """A UV sphere of `radius` centered at the origin (fixed stacks/slices)."""
    import math

    stacks, slices = _SPHERE_STACKS, _RADIAL_SEGMENTS
    verts, normals, faces = [], [], []
    for st in range(stacks + 1):
        phi = math.pi * (st / stacks)
        for sl in range(slices + 1):
            theta = 2.0 * math.pi * (sl / slices)
            n = (
                math.sin(phi) * math.cos(theta),
                math.sin(phi) * math.sin(theta),
                math.cos(phi),
            )
            verts.append((n[0] * radius, n[1] * radius, n[2] * radius))
            normals.append(n)
    row = slices + 1
    for st in range(stacks):
        for sl in range(slices):
            a = st * row + sl
            b = a + row
            faces.append((a, b, a + 1))
            faces.append((a + 1, b, b + 1))
    return verts, normals, faces


def _mesh_local(prim):
    """Fan-triangulate a UsdGeomMesh; compute face normals if none authored."""
    mesh = UsdGeom.Mesh(prim)
    pts_attr = mesh.GetPointsAttr().Get()
    counts_attr = mesh.GetFaceVertexCountsAttr().Get()
    idx_attr = mesh.GetFaceVertexIndicesAttr().Get()
    if not pts_attr or not counts_attr or not idx_attr:
        return [], [], []
    points = [(float(p[0]), float(p[1]), float(p[2])) for p in pts_attr]
    verts, normals, faces = [], [], []
    cursor = 0
    for count in counts_attr:
        poly = [int(idx_attr[cursor + k]) for k in range(count)]
        cursor += count
        if count < 3:
            continue
        p0 = points[poly[0]]
        for k in range(1, count - 1):
            p1, p2 = points[poly[k]], points[poly[k + 1]]
            e1 = (p1[0] - p0[0], p1[1] - p0[1], p1[2] - p0[2])
            e2 = (p2[0] - p0[0], p2[1] - p0[1], p2[2] - p0[2])
            n = (
                e1[1] * e2[2] - e1[2] * e2[1],
                e1[2] * e2[0] - e1[0] * e2[2],
                e1[0] * e2[1] - e1[1] * e2[0],
            )
            ln = (n[0] ** 2 + n[1] ** 2 + n[2] ** 2) ** 0.5 or 1.0
            n = (n[0] / ln, n[1] / ln, n[2] / ln)
            b = len(verts)
            verts.extend([p0, p1, p2])
            normals.extend([n, n, n])
            faces.append((b, b + 1, b + 2))
    return verts, normals, faces


def _gprim_local(prim):
    """Tessellate one gprim into local (verts, normals, faces), or None."""
    if prim.IsA(UsdGeom.Cube):
        size = UsdGeom.Cube(prim).GetSizeAttr().Get()
        return _cube_local(float(size if size is not None else 2.0))
    if prim.IsA(UsdGeom.Cylinder):
        cyl = UsdGeom.Cylinder(prim)
        h = cyl.GetHeightAttr().Get()
        r = cyl.GetRadiusAttr().Get()
        axis = cyl.GetAxisAttr().Get()
        return _cylinder_local(
            float(h if h is not None else 2.0),
            float(r if r is not None else 1.0),
            str(axis) if axis else "Z",
        )
    if prim.IsA(UsdGeom.Sphere):
        r = UsdGeom.Sphere(prim).GetRadiusAttr().Get()
        return _sphere_local(float(r if r is not None else 1.0))
    if prim.IsA(UsdGeom.Mesh):
        return _mesh_local(prim)
    return None


def export_mesh(asset_path, prim_path):
    """Phase 5.6 tessellation pass: open a single component payload and emit a
    single triangulated, indexed mesh (points/normals/indices) as one JSON
    object, with each gprim's local transform (relative to the target prim)
    baked in. Deterministic + read-only; never mutates the stage."""
    if not os.path.isfile(asset_path):
        sys.stderr.write(f"error: mesh asset not found: {asset_path}\n")
        sys.exit(2)

    stage = Usd.Stage.Open(asset_path)
    if stage is None:
        sys.stderr.write(f"error: failed to open mesh asset: {asset_path}\n")
        sys.exit(1)

    prim = None
    if prim_path:
        prim = stage.GetPrimAtPath(prim_path)
    if prim is None or not prim.IsValid():
        prim = stage.GetDefaultPrim()
    if prim is None or not prim.IsValid():
        sys.stderr.write(f"error: no target prim in mesh asset: {asset_path}\n")
        sys.exit(1)

    xf_cache = UsdGeom.XformCache(Usd.TimeCode.Default())
    out_points, out_normals, out_indices = [], [], []

    # Deterministic order: PrimRange is a stable depth-first traversal.
    for p in Usd.PrimRange(prim):
        if not p.IsA(UsdGeom.Gprim):
            continue
        local = _gprim_local(p)
        if local is None:
            continue
        verts, normals, faces = local
        # Transform gprim-local geometry into the target prim's space.
        m = xf_cache.ComputeRelativeTransform(p, prim)[0]
        nmat = m.GetInverse().GetTranspose()
        tv = []
        for v in verts:
            w = m.Transform(Gf.Vec3d(v[0], v[1], v[2]))
            tv.append((float(w[0]), float(w[1]), float(w[2])))
        tn = []
        for n in normals:
            d = nmat.TransformDir(Gf.Vec3d(n[0], n[1], n[2]))
            ln = (d[0] ** 2 + d[1] ** 2 + d[2] ** 2) ** 0.5 or 1.0
            tn.append((float(d[0] / ln), float(d[1] / ln), float(d[2] / ln)))
        _add_tri_soup(out_points, out_normals, out_indices, tv, tn, faces)

    record = {
        "points": out_points,
        "normals": out_normals,
        "indices": out_indices,
    }
    sys.stdout.write(json.dumps(record, separators=(",", ":")))
    sys.stdout.write("\n")
    sys.stdout.flush()


def export_payload(asset_path, prim_path):
    """Phase 2 payload-load pass: open a single component payload WITH geometry
    and emit its surfaced defaults as one JSON object."""
    if not os.path.isfile(asset_path):
        sys.stderr.write(f"error: payload asset not found: {asset_path}\n")
        sys.exit(2)

    stage = Usd.Stage.Open(asset_path)  # normal load (this IS the component file)
    if stage is None:
        sys.stderr.write(f"error: failed to open payload: {asset_path}\n")
        sys.exit(1)

    prim = None
    if prim_path:
        prim = stage.GetPrimAtPath(prim_path)
    if prim is None or not prim.IsValid():
        prim = stage.GetDefaultPrim()
    if prim is None or not prim.IsValid():
        sys.stderr.write(f"error: no target prim in payload: {asset_path}\n")
        sys.exit(1)

    vf = read_vf_customdata(prim)
    kind = prim.GetMetadata("kind")

    bbox = None
    try:
        cache = UsdGeom.BBoxCache(
            Usd.TimeCode.Default(),
            [UsdGeom.Tokens.default_, UsdGeom.Tokens.render],
        )
        world = cache.ComputeWorldBound(prim)
        rng = world.ComputeAlignedRange()
        if not rng.IsEmpty():
            mn, mx = rng.GetMin(), rng.GetMax()
            bbox = [
                [float(mn[0]), float(mn[1]), float(mn[2])],
                [float(mx[0]), float(mx[1]), float(mx[2])],
            ]
    except Exception:  # noqa: BLE001 - bbox is best-effort
        bbox = None

    prim_count = sum(1 for p in Usd.PrimRange(prim) if p.IsA(UsdGeom.Gprim))

    record = {
        "payloadUri": asset_path,
        "primPath": str(prim.GetPath()),
        "kind": str(kind) if kind else None,
        "class": vf.get("class") if isinstance(vf, dict) else None,
        "bbox": bbox,
        "primCount": prim_count,
    }
    sys.stdout.write(json.dumps(record, separators=(",", ":")))
    sys.stdout.write("\n")
    sys.stdout.flush()


def export_root(root):
    if not os.path.isfile(root):
        sys.stderr.write(f"error: USD root layer not found: {root}\n")
        sys.exit(2)
    root_dir = os.path.dirname(os.path.abspath(root))

    # The critical invariant: compose WITHOUT loading payloads.
    stage = Usd.Stage.Open(root, load=Usd.Stage.LoadNone)
    if stage is None:
        sys.stderr.write(f"error: failed to open stage: {root}\n")
        sys.exit(1)

    # Visit instance prims even though their payloads are unloaded: drop the
    # default predicate's IsLoaded requirement.
    predicate = Usd.PrimIsActive & Usd.PrimIsDefined & ~Usd.PrimIsAbstract

    out = sys.stdout
    for prim in stage.Traverse(predicate):
        path = prim.GetPath()
        parent_path = prim.GetParent().GetPath()
        parent = None if parent_path.IsAbsoluteRootPath() else str(parent_path)

        record = {
            "primPath": str(path),
            "kind": prim_kind(prim),
            "parent": parent,
            "vf": read_vf_customdata(prim),
            "bindings": read_bindings(prim),
        }
        t = read_translate(prim)
        if t is not None:
            record["transform"] = {"translate": t}
        eh = read_extents_hint(prim)
        if eh is not None:
            record["extentsHint"] = eh
        geom = read_payload(prim, root_dir)
        if geom is not None:
            record["geomRef"] = geom

        out.write(json.dumps(record, separators=(",", ":")))
        out.write("\n")

    out.flush()


def main():
    args = sys.argv[1:]
    if args and args[0] == "--payload":
        if len(args) < 2:
            sys.stderr.write("usage: usd_export.py --payload <asset> [primPath]\n")
            sys.exit(2)
        prim_path = args[2] if len(args) > 2 else ""
        export_payload(args[1], prim_path)
        return

    if args and args[0] == "--mesh":
        if len(args) < 2:
            sys.stderr.write("usage: usd_export.py --mesh <asset> [primPath]\n")
            sys.exit(2)
        prim_path = args[2] if len(args) > 2 else ""
        export_mesh(args[1], prim_path)
        return

    if len(args) != 1:
        sys.stderr.write(
            "usage: usd_export.py <usd_root_layer>\n"
            "       usd_export.py --payload <asset> [primPath]\n"
            "       usd_export.py --mesh <asset> [primPath]\n"
        )
        sys.exit(2)
    export_root(args[0])


if __name__ == "__main__":
    main()
