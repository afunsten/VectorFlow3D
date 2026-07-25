// WebGPU renderer (spec §3.6 WebGPU tier). Each RenderScene entity draws as
// either a real tessellated mesh from the VF geometry store (Phase 5.6) or, when
// no mesh is available (no GeomRef, unresolved hash, or a mesh not yet fetched),
// the Phase 5.5 AABB **proxy box** — the LOD-0 fallback that guarantees an entity
// always renders. Meshes and boxes share the telemetry-quality tint, the
// selection highlight, and (for meshes) a dark edge outline. A box is positioned
// at `transform.translation + extentsCenter`; a mesh is drawn at the entity's
// full transform (the tessellation baked in the component-local gprim xforms).

import type { OrbitCamera } from "./camera";
import type { RenderEntity, RenderScene } from "./renderScene";
import type { EntityId, MeshData, VisualSample } from "./protocol";
import { translationOf } from "./protocol";

const FLOATS_PER_INSTANCE = 10; // center(3) half(3) color(3) selected(1)
const MAX_INSTANCES = 200_000;

const MESH_FLOATS_PER_INSTANCE = 20; // model(16) color(3) selected(1)
const MAX_MESH_INSTANCES = 50_000;
const OUTLINE_EXPAND = 0.02; // metres pushed along the normal for the edge rim

// Quality -> tint (see index.html legend).
const OK: [number, number, number] = [0.29, 0.87, 0.5];
const STALE: [number, number, number] = [0.98, 0.75, 0.14];
const UNAVAILABLE: [number, number, number] = [0.97, 0.44, 0.44];
const NO_TELEMETRY: [number, number, number] = [0.42, 0.48, 0.56];

function tintFor(visual: VisualSample[]): [number, number, number] {
  if (visual.length === 0) return NO_TELEMETRY;
  let worst = 0; // 0 ok, 1 stale, 2 unavailable/error
  for (const v of visual) {
    if (v.quality === "unavailable" || v.quality === "error") worst = Math.max(worst, 2);
    else if (v.quality === "stale") worst = Math.max(worst, 1);
  }
  return worst === 2 ? UNAVAILABLE : worst === 1 ? STALE : OK;
}

const SHADER = /* wgsl */ `
struct Uniforms {
  viewProj : mat4x4<f32>,
  lightDir : vec4<f32>,
};
@group(0) @binding(0) var<uniform> u : Uniforms;

struct VSOut {
  @builtin(position) clip : vec4<f32>,
  @location(0) normal : vec3<f32>,
  @location(1) color  : vec3<f32>,
  @location(2) selected : f32,
};

@vertex
fn vs(
  @location(0) pos : vec3<f32>,
  @location(1) normal : vec3<f32>,
  @location(2) center : vec3<f32>,
  @location(3) half : vec3<f32>,
  @location(4) color : vec3<f32>,
  @location(5) selected : f32,
) -> VSOut {
  var out : VSOut;
  let world = center + pos * (half * 2.0);
  out.clip = u.viewProj * vec4<f32>(world, 1.0);
  out.normal = normal;
  out.color = color;
  out.selected = selected;
  return out;
}

@fragment
fn fs(in : VSOut) -> @location(0) vec4<f32> {
  let n = normalize(in.normal);
  let shade = 0.35 + 0.65 * max(dot(n, normalize(u.lightDir.xyz)), 0.0);
  var c = in.color * shade;
  if (in.selected > 0.5) {
    c = mix(c, vec3<f32>(1.0, 1.0, 1.0), 0.55);
  }
  return vec4<f32>(c, 1.0);
}
`;

// Mesh + edge-outline shader. Per-entity model matrix rides as instance data so
// entities sharing one store mesh draw as instances of a single vertex buffer.
const MESH_SHADER = /* wgsl */ `
struct Uniforms {
  viewProj : mat4x4<f32>,
  lightDir : vec4<f32>,
  outline  : vec4<f32>,   // x = outline expand (0 = fill pass)
};
@group(0) @binding(0) var<uniform> u : Uniforms;

struct VSOut {
  @builtin(position) clip : vec4<f32>,
  @location(0) normal : vec3<f32>,
  @location(1) color  : vec3<f32>,
  @location(2) selected : f32,
};

@vertex
fn vs(
  @location(0) pos : vec3<f32>,
  @location(1) normal : vec3<f32>,
  @location(2) m0 : vec4<f32>,
  @location(3) m1 : vec4<f32>,
  @location(4) m2 : vec4<f32>,
  @location(5) m3 : vec4<f32>,
  @location(6) color : vec3<f32>,
  @location(7) selected : f32,
) -> VSOut {
  let model = mat4x4<f32>(m0, m1, m2, m3);
  let local = pos + normal * u.outline.x;
  let world = model * vec4<f32>(local, 1.0);
  var out : VSOut;
  out.clip = u.viewProj * world;
  out.normal = (model * vec4<f32>(normal, 0.0)).xyz;
  out.color = color;
  out.selected = selected;
  return out;
}

@fragment
fn fs(in : VSOut) -> @location(0) vec4<f32> {
  // Outline pass: flat dark rim.
  if (u.outline.x > 0.0) {
    return vec4<f32>(0.02, 0.04, 0.06, 1.0);
  }
  let n = normalize(in.normal);
  let shade = 0.35 + 0.65 * max(dot(n, normalize(u.lightDir.xyz)), 0.0);
  var c = in.color * shade;
  if (in.selected > 0.5) {
    c = mix(c, vec3<f32>(1.0, 1.0, 1.0), 0.55);
  }
  return vec4<f32>(c, 1.0);
}
`;

// 24 verts (4 per face) with per-face normals; 36 indices.
function cubeGeometry(): { verts: Float32Array; indices: Uint16Array } {
  const p = 0.5;
  const faces: { n: [number, number, number]; v: [number, number, number][] }[] = [
    { n: [0, 0, 1], v: [[-p, -p, p], [p, -p, p], [p, p, p], [-p, p, p]] },
    { n: [0, 0, -1], v: [[p, -p, -p], [-p, -p, -p], [-p, p, -p], [p, p, -p]] },
    { n: [1, 0, 0], v: [[p, -p, p], [p, -p, -p], [p, p, -p], [p, p, p]] },
    { n: [-1, 0, 0], v: [[-p, -p, -p], [-p, -p, p], [-p, p, p], [-p, p, -p]] },
    { n: [0, 1, 0], v: [[-p, p, p], [p, p, p], [p, p, -p], [-p, p, -p]] },
    { n: [0, -1, 0], v: [[-p, -p, -p], [p, -p, -p], [p, -p, p], [-p, -p, p]] },
  ];
  const verts: number[] = [];
  const indices: number[] = [];
  faces.forEach((f, fi) => {
    const base = fi * 4;
    for (const v of f.v) verts.push(v[0], v[1], v[2], f.n[0], f.n[1], f.n[2]);
    indices.push(base, base + 1, base + 2, base, base + 2, base + 3);
  });
  return { verts: new Float32Array(verts), indices: new Uint16Array(indices) };
}

/** One store mesh uploaded to the GPU (shared by every entity with that hash). */
interface GpuMesh {
  vbuf: GPUBuffer;
  ibuf: GPUBuffer;
  indexCount: number;
}

export class Renderer {
  private device!: GPUDevice;
  private context!: GPUCanvasContext;
  private format!: GPUTextureFormat;
  private pipeline!: GPURenderPipeline;
  private cubeBuf!: GPUBuffer;
  private indexBuf!: GPUBuffer;
  private indexCount = 0;
  private instanceBuf!: GPUBuffer;
  private uniformBuf!: GPUBuffer;
  private bindGroup!: GPUBindGroup;
  private depth!: GPUTexture;
  private instanceData = new Float32Array(MAX_INSTANCES * FLOATS_PER_INSTANCE);

  // Mesh path (Phase 5.6).
  private meshPipeline!: GPURenderPipeline;
  private outlinePipeline!: GPURenderPipeline;
  private meshInstanceBuf!: GPUBuffer;
  private meshInstanceData = new Float32Array(MAX_MESH_INSTANCES * MESH_FLOATS_PER_INSTANCE);
  private meshUniformBuf!: GPUBuffer; // fill pass (outline = 0)
  private outlineUniformBuf!: GPUBuffer; // outline pass (outline > 0)
  private meshBindGroup!: GPUBindGroup;
  private outlineBindGroup!: GPUBindGroup;
  private meshes = new Map<string, GpuMesh>();

  constructor(private canvas: HTMLCanvasElement) {}

  async init(): Promise<void> {
    if (!navigator.gpu) throw new Error("WebGPU not available in this browser");
    const adapter = await navigator.gpu.requestAdapter();
    if (!adapter) throw new Error("no WebGPU adapter");
    this.device = await adapter.requestDevice();
    const ctx = this.canvas.getContext("webgpu");
    if (!ctx) throw new Error("could not get a WebGPU canvas context");
    this.context = ctx;
    this.format = navigator.gpu.getPreferredCanvasFormat();
    this.context.configure({ device: this.device, format: this.format, alphaMode: "opaque" });

    const geo = cubeGeometry();
    this.cubeBuf = this.makeBuffer(geo.verts, GPUBufferUsage.VERTEX);
    this.indexBuf = this.makeBuffer(geo.indices, GPUBufferUsage.INDEX);
    this.indexCount = geo.indices.length;
    this.instanceBuf = this.device.createBuffer({
      size: this.instanceData.byteLength,
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });
    this.uniformBuf = this.device.createBuffer({
      size: 80, // mat4 (64) + vec4 (16)
      usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST,
    });

    const module = this.device.createShaderModule({ code: SHADER });
    this.pipeline = this.device.createRenderPipeline({
      layout: "auto",
      vertex: {
        module,
        entryPoint: "vs",
        buffers: [
          {
            arrayStride: 24,
            attributes: [
              { shaderLocation: 0, offset: 0, format: "float32x3" },
              { shaderLocation: 1, offset: 12, format: "float32x3" },
            ],
          },
          {
            arrayStride: FLOATS_PER_INSTANCE * 4,
            stepMode: "instance",
            attributes: [
              { shaderLocation: 2, offset: 0, format: "float32x3" },
              { shaderLocation: 3, offset: 12, format: "float32x3" },
              { shaderLocation: 4, offset: 24, format: "float32x3" },
              { shaderLocation: 5, offset: 36, format: "float32" },
            ],
          },
        ],
      },
      fragment: { module, entryPoint: "fs", targets: [{ format: this.format }] },
      primitive: { topology: "triangle-list", cullMode: "back" },
      depthStencil: { format: "depth24plus", depthWriteEnabled: true, depthCompare: "less" },
    });

    this.bindGroup = this.device.createBindGroup({
      layout: this.pipeline.getBindGroupLayout(0),
      entries: [{ binding: 0, resource: { buffer: this.uniformBuf } }],
    });

    this.initMeshPipeline();
    this.resize();
  }

  /** Mesh (fill) + outline (front-cull, expanded) pipelines share one module. */
  private initMeshPipeline(): void {
    this.meshInstanceBuf = this.device.createBuffer({
      size: this.meshInstanceData.byteLength,
      usage: GPUBufferUsage.VERTEX | GPUBufferUsage.COPY_DST,
    });
    this.meshUniformBuf = this.device.createBuffer({
      size: 96, // mat4 (64) + vec4 (16) + vec4 (16)
      usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST,
    });
    this.outlineUniformBuf = this.device.createBuffer({
      size: 96,
      usage: GPUBufferUsage.UNIFORM | GPUBufferUsage.COPY_DST,
    });

    const module = this.device.createShaderModule({ code: MESH_SHADER });
    const vertexState: GPUVertexState = {
      module,
      entryPoint: "vs",
      buffers: [
        {
          arrayStride: 24,
          attributes: [
            { shaderLocation: 0, offset: 0, format: "float32x3" },
            { shaderLocation: 1, offset: 12, format: "float32x3" },
          ],
        },
        {
          arrayStride: MESH_FLOATS_PER_INSTANCE * 4,
          stepMode: "instance",
          attributes: [
            { shaderLocation: 2, offset: 0, format: "float32x4" },
            { shaderLocation: 3, offset: 16, format: "float32x4" },
            { shaderLocation: 4, offset: 32, format: "float32x4" },
            { shaderLocation: 5, offset: 48, format: "float32x4" },
            { shaderLocation: 6, offset: 64, format: "float32x3" },
            { shaderLocation: 7, offset: 76, format: "float32" },
          ],
        },
      ],
    };
    const depthStencil: GPUDepthStencilState = {
      format: "depth24plus",
      depthWriteEnabled: true,
      depthCompare: "less",
    };
    this.meshPipeline = this.device.createRenderPipeline({
      layout: "auto",
      vertex: vertexState,
      fragment: { module, entryPoint: "fs", targets: [{ format: this.format }] },
      primitive: { topology: "triangle-list", cullMode: "back" },
      depthStencil,
    });
    // Outline: cull front faces of an outward-expanded shell to leave a rim.
    this.outlinePipeline = this.device.createRenderPipeline({
      layout: "auto",
      vertex: vertexState,
      fragment: { module, entryPoint: "fs", targets: [{ format: this.format }] },
      primitive: { topology: "triangle-list", cullMode: "front" },
      depthStencil,
    });
    this.meshBindGroup = this.device.createBindGroup({
      layout: this.meshPipeline.getBindGroupLayout(0),
      entries: [{ binding: 0, resource: { buffer: this.meshUniformBuf } }],
    });
    this.outlineBindGroup = this.device.createBindGroup({
      layout: this.outlinePipeline.getBindGroupLayout(0),
      entries: [{ binding: 0, resource: { buffer: this.outlineUniformBuf } }],
    });
  }

  /** Upload a store mesh once, keyed by its content hash. Idempotent. */
  setMesh(hash: string, mesh: MeshData): void {
    if (this.meshes.has(hash)) return;
    // Interleave position + normal into one vertex buffer (stride 24).
    const nverts = mesh.positions.length / 3;
    const interleaved = new Float32Array(nverts * 6);
    for (let i = 0; i < nverts; i++) {
      interleaved[i * 6 + 0] = mesh.positions[i * 3 + 0];
      interleaved[i * 6 + 1] = mesh.positions[i * 3 + 1];
      interleaved[i * 6 + 2] = mesh.positions[i * 3 + 2];
      interleaved[i * 6 + 3] = mesh.normals[i * 3 + 0] ?? 0;
      interleaved[i * 6 + 4] = mesh.normals[i * 3 + 1] ?? 0;
      interleaved[i * 6 + 5] = mesh.normals[i * 3 + 2] ?? 0;
    }
    const vbuf = this.makeBuffer(interleaved, GPUBufferUsage.VERTEX);
    const ibuf = this.makeBuffer(mesh.indices, GPUBufferUsage.INDEX);
    this.meshes.set(hash, { vbuf, ibuf, indexCount: mesh.indices.length });
  }

  hasMesh(hash: string): boolean {
    return this.meshes.has(hash);
  }

  /** Drop all uploaded meshes (on reconnect the store re-delivers them). */
  clearMeshes(): void {
    for (const m of this.meshes.values()) {
      m.vbuf.destroy();
      m.ibuf.destroy();
    }
    this.meshes.clear();
  }

  private makeBuffer(
    data: Float32Array | Uint16Array | Uint32Array,
    usage: GPUBufferUsageFlags,
  ): GPUBuffer {
    const buf = this.device.createBuffer({ size: data.byteLength, usage, mappedAtCreation: true });
    const range = buf.getMappedRange();
    if (data instanceof Float32Array) new Float32Array(range).set(data);
    else if (data instanceof Uint32Array) new Uint32Array(range).set(data);
    else new Uint16Array(range).set(data);
    buf.unmap();
    return buf;
  }

  resize(): void {
    const dpr = Math.min(window.devicePixelRatio || 1, 2);
    const w = Math.max(1, Math.floor(this.canvas.clientWidth * dpr));
    const h = Math.max(1, Math.floor(this.canvas.clientHeight * dpr));
    if (this.canvas.width === w && this.canvas.height === h && this.depth) return;
    this.canvas.width = w;
    this.canvas.height = h;
    this.depth?.destroy();
    this.depth = this.device.createTexture({
      size: [w, h],
      format: "depth24plus",
      usage: GPUTextureUsage.RENDER_ATTACHMENT,
    });
  }

  get aspect(): number {
    return this.canvas.width / Math.max(1, this.canvas.height);
  }

  /** Build per-instance data for boxes + meshes and issue the draws. */
  render(scene: RenderScene, camera: OrbitCamera, selected: EntityId | null): void {
    this.resize();

    // Partition entities: real mesh (hash uploaded) vs proxy box (fallback).
    const meshGroups = new Map<string, RenderEntity[]>();
    const boxEntities: RenderEntity[] = [];
    for (const e of scene.entities()) {
      const hash = e.geomRef?.content_hash;
      if (hash && this.meshes.has(hash)) {
        let g = meshGroups.get(hash);
        if (!g) meshGroups.set(hash, (g = []));
        g.push(e);
      } else {
        boxEntities.push(e);
      }
    }

    // ---- Box instance buffer (LOD-0 fallback). ----
    let n = 0;
    for (const e of boxEntities) {
      if (n >= MAX_INSTANCES) break;
      const box = boxOf(e);
      if (!box) continue;
      const tint = tintFor(e.visual);
      const off = n * FLOATS_PER_INSTANCE;
      this.instanceData[off + 0] = box.center[0];
      this.instanceData[off + 1] = box.center[1];
      this.instanceData[off + 2] = box.center[2];
      this.instanceData[off + 3] = box.half[0];
      this.instanceData[off + 4] = box.half[1];
      this.instanceData[off + 5] = box.half[2];
      this.instanceData[off + 6] = tint[0];
      this.instanceData[off + 7] = tint[1];
      this.instanceData[off + 8] = tint[2];
      this.instanceData[off + 9] = e.id === selected ? 1 : 0;
      n++;
    }
    if (n > 0) {
      this.device.queue.writeBuffer(this.instanceBuf, 0, this.instanceData, 0, n * FLOATS_PER_INSTANCE);
    }

    // ---- Mesh instance buffer (grouped by hash, one draw per mesh). ----
    const drawRanges: { mesh: GpuMesh; offsetFloats: number; count: number }[] = [];
    let mi = 0;
    for (const [hash, ents] of meshGroups) {
      const mesh = this.meshes.get(hash)!;
      const startFloats = mi * MESH_FLOATS_PER_INSTANCE;
      let count = 0;
      for (const e of ents) {
        if (mi >= MAX_MESH_INSTANCES) break;
        const tint = tintFor(e.visual);
        const off = mi * MESH_FLOATS_PER_INSTANCE;
        const m = columnMajor(e.transform.matrix);
        this.meshInstanceData.set(m, off);
        this.meshInstanceData[off + 16] = tint[0];
        this.meshInstanceData[off + 17] = tint[1];
        this.meshInstanceData[off + 18] = tint[2];
        this.meshInstanceData[off + 19] = e.id === selected ? 1 : 0;
        mi++;
        count++;
      }
      if (count > 0) drawRanges.push({ mesh, offsetFloats: startFloats, count });
    }
    if (mi > 0) {
      this.device.queue.writeBuffer(this.meshInstanceBuf, 0, this.meshInstanceData, 0, mi * MESH_FLOATS_PER_INSTANCE);
    }

    // ---- Uniforms. ----
    const vp = camera.viewProj(this.aspect);
    const light: [number, number, number, number] = [0.4, 0.5, 0.85, 0.0];
    const uni = new Float32Array(20);
    uni.set(vp, 0);
    uni.set(light, 16);
    this.device.queue.writeBuffer(this.uniformBuf, 0, uni);

    const meshUni = new Float32Array(24);
    meshUni.set(vp, 0);
    meshUni.set(light, 16);
    meshUni.set([0, 0, 0, 0], 20); // outline off (fill)
    this.device.queue.writeBuffer(this.meshUniformBuf, 0, meshUni);
    const outlineUni = meshUni.slice();
    outlineUni.set([OUTLINE_EXPAND, 0, 0, 0], 20);
    this.device.queue.writeBuffer(this.outlineUniformBuf, 0, outlineUni);

    // ---- Draw. ----
    const encoder = this.device.createCommandEncoder();
    const pass = encoder.beginRenderPass({
      colorAttachments: [
        {
          view: this.context.getCurrentTexture().createView(),
          clearValue: { r: 0.043, g: 0.059, b: 0.078, a: 1 },
          loadOp: "clear",
          storeOp: "store",
        },
      ],
      depthStencilAttachment: {
        view: this.depth.createView(),
        depthClearValue: 1.0,
        depthLoadOp: "clear",
        depthStoreOp: "store",
      },
    });

    if (n > 0) {
      pass.setPipeline(this.pipeline);
      pass.setBindGroup(0, this.bindGroup);
      pass.setVertexBuffer(0, this.cubeBuf);
      pass.setVertexBuffer(1, this.instanceBuf);
      pass.setIndexBuffer(this.indexBuf, "uint16");
      pass.drawIndexed(this.indexCount, n);
    }

    if (drawRanges.length > 0) {
      // Outline pass first, then the fill draws over the interior.
      pass.setPipeline(this.outlinePipeline);
      pass.setBindGroup(0, this.outlineBindGroup);
      for (const r of drawRanges) {
        pass.setVertexBuffer(0, r.mesh.vbuf);
        pass.setVertexBuffer(1, this.meshInstanceBuf, r.offsetFloats * 4);
        pass.setIndexBuffer(r.mesh.ibuf, "uint32");
        pass.drawIndexed(r.mesh.indexCount, r.count);
      }
      pass.setPipeline(this.meshPipeline);
      pass.setBindGroup(0, this.meshBindGroup);
      for (const r of drawRanges) {
        pass.setVertexBuffer(0, r.mesh.vbuf);
        pass.setVertexBuffer(1, this.meshInstanceBuf, r.offsetFloats * 4);
        pass.setIndexBuffer(r.mesh.ibuf, "uint32");
        pass.drawIndexed(r.mesh.indexCount, r.count);
      }
    }

    pass.end();
    this.device.queue.submit([encoder.finish()]);
  }
}

/** Row-major (wire) 4x4 -> column-major (WGSL) Float32Array. */
function columnMajor(rows: number[]): Float32Array {
  const m = new Float32Array(16);
  for (let r = 0; r < 4; r++) {
    for (let c = 0; c < 4; c++) {
      m[c * 4 + r] = rows[r * 4 + c];
    }
  }
  return m;
}

/** World box (center + half) for an entity: origin-relative extents anchored at
 *  the (possibly pinned) transform translation. */
function boxOf(e: RenderEntity): { center: [number, number, number]; half: [number, number, number] } | null {
  const t = translationOf(e.transform);
  const ext = e.extents;
  if (!ext) {
    // No authored extents: draw a small unit marker at the translation.
    return { center: t, half: [0.5, 0.5, 0.5] };
  }
  const cx = (ext.min[0] + ext.max[0]) / 2;
  const cy = (ext.min[1] + ext.max[1]) / 2;
  const cz = (ext.min[2] + ext.max[2]) / 2;
  const half: [number, number, number] = [
    Math.max((ext.max[0] - ext.min[0]) / 2, 0.05),
    Math.max((ext.max[1] - ext.min[1]) / 2, 0.05),
    Math.max((ext.max[2] - ext.min[2]) / 2, 0.05),
  ];
  return { center: [t[0] + cx, t[1] + cy, t[2] + cz], half };
}
