// Orbit camera for a Z-up world (spec §3.0: stages are Z-up). Produces the
// view-projection matrix for the renderer, derives the observer's AOI sphere
// (centered on the look-at target), and unprojects a screen click into a world
// ray for PickRequest.

import type { Mat4, Vec3 } from "./mat4";
import { add, cross, invert, lookAt, multiply, normalize, perspective, scale, sub, transformPoint } from "./mat4";

const UP: Vec3 = [0, 0, 1];

export class OrbitCamera {
  target: Vec3 = [0, 0, 0];
  distance = 26;
  yaw = 0.9; // azimuth (rad)
  pitch = 0.6; // elevation above the XY plane (rad)
  fovy = (55 * Math.PI) / 180;
  near = 0.1;
  far = 5000;
  /** AOI sphere radius the observer requests around the target. */
  aoiRadius = 10;

  eye(): Vec3 {
    const cp = Math.cos(this.pitch);
    const dir: Vec3 = [cp * Math.cos(this.yaw), cp * Math.sin(this.yaw), Math.sin(this.pitch)];
    return add(this.target, scale(dir, this.distance));
  }

  viewProj(aspect: number): Mat4 {
    const proj = perspective(this.fovy, aspect, this.near, this.far);
    const view = lookAt(this.eye(), this.target, UP);
    return multiply(proj, view);
  }

  orbit(dxPix: number, dyPix: number): void {
    const s = 0.006;
    this.yaw -= dxPix * s;
    this.pitch += dyPix * s;
    const lim = Math.PI / 2 - 0.05;
    this.pitch = Math.max(-lim, Math.min(lim, this.pitch));
  }

  zoom(deltaY: number): void {
    this.distance *= Math.exp(deltaY * 0.001);
    this.distance = Math.max(2, Math.min(2000, this.distance));
  }

  /** Pan the target (and thus the AOI) in the camera's right/up plane. */
  pan(dxPix: number, dyPix: number): void {
    const forward = normalize(sub(this.target, this.eye()));
    const right = normalize(cross(forward, UP));
    const up = normalize(cross(right, forward));
    const k = this.distance * 0.0015;
    this.target = add(this.target, add(scale(right, -dxPix * k), scale(up, dyPix * k)));
  }

  /** AOI sphere center = the look-at target (spec §3.2 camera AOI). */
  aoiCenter(): Vec3 {
    return this.target;
  }

  /** World-space ray for a click at normalized device coords (x,y in [-1,1]). */
  rayFromNdc(ndcX: number, ndcY: number, aspect: number): { origin: Vec3; dir: Vec3 } {
    const invVP = invert(this.viewProj(aspect));
    const near = transformPoint(invVP, [ndcX, ndcY, -1]);
    const far = transformPoint(invVP, [ndcX, ndcY, 1]);
    return { origin: near, dir: normalize(sub(far, near)) };
  }
}
