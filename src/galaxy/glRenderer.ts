// WebGL 渲染器（需求 REQ-103 / REQ-104 / REQ-108）。
//
// 目标只有一个：把「几万个圆 + 几万条线」一次性提交给 GPU，而不是像 Canvas 2D
// 那样每个节点都走一遍路径构造。做法是最朴素的点精灵（gl.POINTS）：
//
//   * 每个节点 = 1 个顶点，颜色/半径/透明度走顶点属性，圆形靠 fragment shader
//     里 discard 掉半径外的像素（比"贴纹理"省内存，也不用准备素材）。
//   * 每条边 = 2 个顶点，用 gl.LINES 一次画完。
//   * 相机只改 uniform 里的平移/缩放，CPU 侧不重算坐标。
//
// 为什么不用 PixiJS：见 capability.ts 顶部说明。这里只需要点与线两种图元，
// 自绘反而更容易在驱动异常时精确回退到 Canvas 2D。

import { BLUEPRINT_CONFIG } from "./model";
import type { GEdge, GNode } from "./model";
import type { QualityProfile, WebGLSupport } from "./capability";

export interface GLFrameInput {
  nodes: GNode[];
  edges: GEdge[];
  camera: { x: number; y: number; k: number };
  width: number;
  height: number;
  dpr: number;
  selectedId: string | null;
  hoveredId: string | null;
  focusId: string | null;
  neighbors: Set<string>;
  /** 鱼眼透镜参数；null 表示关闭（REQ-077）。 */
  fisheye?: { on: boolean; x: number; y: number; radius: number; scale: number } | null;
}

// 顶点着色器：世界坐标 -> 裁剪空间。y 取反是因为屏幕坐标 y 向下、GL 向上。
// compat 版本把 highp 换成 mediump：老 Intel/AMD 驱动上 highp 顶点精度会直接
// 编译失败（REQ-104 的"兼容版着色器"）。
function vertexShader(highp: boolean): string {
  const prec = highp ? "highp" : "mediump";
  return [
    "precision " + prec + " float;",
    "attribute vec2 a_pos;",
    "attribute vec4 a_color;",
    "attribute float a_size;",
    "uniform vec2 u_center;",
    "uniform vec2 u_viewport;",
    "uniform float u_k;",
    "uniform float u_dpr;",
    // 鱼眼透镜（REQ-077）：u_fish = (开关, 鼠标x, 鼠标y, 半径)，单位是设备无关像素
    "uniform vec4 u_fish;",
    "uniform float u_fishScale;",
    "varying vec4 v_color;",
    "void main() {",
    "  vec2 p = (a_pos - u_center) * u_k;",
    "  float size = a_size * u_k;",
    "  if (u_fish.x > 0.5) {",
    "    vec2 s = p + u_viewport * 0.5;",
    "    vec2 d = s - u_fish.yz;",
    "    float dist = length(d);",
    "    if (dist < u_fish.w && dist > 0.001) {",
    "      float t = 1.0 - dist / u_fish.w;",
    "      s += (d / dist) * (t * 42.0);",
    "      size *= 1.0 + (u_fishScale - 1.0) * t;",
    "    }",
    "    p = s - u_viewport * 0.5;",
    "  }",
    "  gl_Position = vec4(p.x * 2.0 / u_viewport.x, -p.y * 2.0 / u_viewport.y, 0.0, 1.0);",
    "  gl_PointSize = max(1.0, size * u_dpr * 2.0);",
    "  v_color = a_color;",
    "}",
  ].join("\n");
}

// 片元着色器：把方形点裁成圆。
// 蓝图风要求"硬边"，柔光衰减会让浅色底上的点糊成一团，因此只保留 1px 抗锯齿。
function nodeFragmentShader(): string {
  return [
    "precision mediump float;",
    "varying vec4 v_color;",
    "void main() {",
    "  vec2 d = gl_PointCoord * 2.0 - 1.0;",
    "  float r = length(d);",
    "  if (r > 1.0) discard;",
    "  float a = v_color.a * (1.0 - smoothstep(0.90, 1.0, r));",
    "  gl_FragColor = vec4(v_color.rgb, a);",
    "}",
  ].join("\n");
}

const LINE_VS = [
  "precision mediump float;",
  "attribute vec2 a_pos;",
  "attribute vec4 a_color;",
  "uniform vec2 u_center;",
  "uniform vec2 u_viewport;",
  "uniform float u_k;",
  "varying vec4 v_color;",
  "void main() {",
  "  vec2 p = (a_pos - u_center) * u_k;",
  "  gl_Position = vec4(p.x * 2.0 / u_viewport.x, -p.y * 2.0 / u_viewport.y, 0.0, 1.0);",
  "  v_color = a_color;",
  "}",
].join("\n");

const LINE_FS = [
  "precision mediump float;",
  "varying vec4 v_color;",
  "void main() { gl_FragColor = v_color; }",
].join("\n");

function compile(gl: WebGLRenderingContext | WebGL2RenderingContext, type: number, source: string) {
  const sh = gl.createShader(type);
  if (!sh) return null;
  gl.shaderSource(sh, source);
  gl.compileShader(sh);
  if (!gl.getShaderParameter(sh, gl.COMPILE_STATUS)) {
    // 不抛异常：调用方要能"编译失败就换兼容版着色器"（REQ-104）。
    gl.deleteShader(sh);
    return null;
  }
  return sh;
}

function link(gl: WebGLRenderingContext | WebGL2RenderingContext, vs: string, fs: string) {
  const v = compile(gl, gl.VERTEX_SHADER, vs);
  const f = compile(gl, gl.FRAGMENT_SHADER, fs);
  if (!v || !f) return null;
  const p = gl.createProgram();
  if (!p) return null;
  gl.attachShader(p, v);
  gl.attachShader(p, f);
  gl.linkProgram(p);
  gl.deleteShader(v);
  gl.deleteShader(f);
  if (!gl.getProgramParameter(p, gl.LINK_STATUS)) {
    gl.deleteProgram(p);
    return null;
  }
  return p;
}

export class GLGalaxyRenderer {
  private canvas: HTMLCanvasElement;
  private gl: WebGLRenderingContext | WebGL2RenderingContext;
  private support: WebGLSupport;
  private profile: QualityProfile;

  private nodeProgram: WebGLProgram | null = null;
  private lineProgram: WebGLProgram | null = null;
  /** 实际生效的着色器精度（编译失败会从 highp 掉到 mediump）。 */
  shaderPrecision = "highp";

  private nodeBuf: WebGLBuffer | null = null;
  private lineBuf: WebGLBuffer | null = null;

  // 顶点属性布局：pos(2) + color(4) + size(1) = 7 floats / 节点
  private static readonly NODE_STRIDE = 7;
  private static readonly LINE_STRIDE = 6; // pos(2) + color(4)

  /** 上一帧真实提交耗时（毫秒），用于"超过 16ms 提示降级"（REQ-103）。 */
  lastFrameMs = 0;
  /** 是否已经因为掉帧触发过降级建议（只提示一次，不要反复烦人）。 */
  slowFrameHintFired = false;

  constructor(canvas: HTMLCanvasElement, support: WebGLSupport, profile: QualityProfile) {
    this.canvas = canvas;
    this.support = support;
    this.profile = profile;
    const opts: WebGLContextAttributes = {
      alpha: true,
      antialias: false,
      depth: false,
      stencil: false,
      premultipliedAlpha: true,
      // 主线程渲染，不要额外占用一个 rAF 线程；也不需要保留下次绘制内容。
      preserveDrawingBuffer: false,
    };
    let gl = canvas.getContext("webgl2", opts) as WebGL2RenderingContext | null;
    if (!gl) {
      gl = canvas.getContext("webgl", opts) as WebGLRenderingContext | null;
    }
    if (!gl) throw new Error("WebGL 上下文创建失败");
    this.gl = gl;
    this.setup();
  }

  setProfile(profile: QualityProfile) {
    if (this.profile === profile) return;
    this.profile = profile;
    this.setup();
  }

  private setup() {
    const gl = this.gl;
    const highp = this.support.precision === "highp";
    // 先用高性能着色器；失败就退兼容版（REQ-104 的 compile_shader_safe）。
    this.nodeProgram = link(gl, vertexShader(highp), nodeFragmentShader());
    this.shaderPrecision = highp ? "highp" : "mediump";
    if (!this.nodeProgram) {
      this.nodeProgram = link(gl, vertexShader(false), nodeFragmentShader());
      this.shaderPrecision = "mediump";
    }
    this.lineProgram = link(gl, LINE_VS, LINE_FS);
    if (!this.lineProgram) {
      throw new Error("连线着色器编译失败");
    }

    if (!this.nodeBuf) this.nodeBuf = gl.createBuffer();
    if (!this.lineBuf) this.lineBuf = gl.createBuffer();

    gl.disable(gl.DEPTH_TEST);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);
  }

  resize(width: number, height: number, dpr: number) {
    const w = Math.max(1, Math.floor(width * dpr));
    const h = Math.max(1, Math.floor(height * dpr));
    if (this.canvas.width !== w) this.canvas.width = w;
    if (this.canvas.height !== h) this.canvas.height = h;
    this.canvas.style.width = width + "px";
    this.canvas.style.height = height + "px";
  }

  /** 每帧调用：上传顶点并绘制。 */
  draw(input: GLFrameInput) {
    const gl = this.gl;
    const t0 = performance.now();
    const { nodes, edges, camera, width, height, dpr, selectedId, hoveredId, focusId, neighbors } = input;

    gl.viewport(0, 0, this.canvas.width, this.canvas.height);
    // 底色也换成蓝图浅色：即使将来有人把后端切回 webgl，
    // 也不会再透出深色星空底。
    const bg = hexToRgb(BLUEPRINT_CONFIG.bgColor);
    gl.clearColor(bg[0], bg[1], bg[2], 1);
    gl.clear(gl.COLOR_BUFFER_BIT);
    gl.enable(gl.BLEND);
    gl.blendFunc(gl.SRC_ALPHA, gl.ONE_MINUS_SRC_ALPHA);

    // ---- 连线
    if (this.lineProgram) {
      const lineData = new Float32Array(edges.length * 2 * GLGalaxyRenderer.LINE_STRIDE);
      let o = 0;
      let count = 0;
      for (const e of edges) {
        const s = (e as unknown as { source: GNode }).source;
        const t = (e as unknown as { target: GNode }).target;
        if (!s || !t || typeof s.x !== "number" || typeof t.x !== "number") continue;
        const related =
          (selectedId !== null && (s.id === selectedId || t.id === selectedId)) ||
          (focusId !== null && (s.id === focusId || t.id === focusId));
        let a = 0.42;
        if (related) {
          a = 0.82;
        } else if (focusId !== null) {
          const near = neighbors.has(s.id) || neighbors.has(t.id);
          if (!near) a = 0.10;
        }
        // 蓝图连线：普通 #5d768f，高亮 #ffeb3b。
        // GL 的 LINES 画不出虚线，"文件边用虚线"只在 canvas2d 路径生效。
        const cr = related ? hexToRgb(BLUEPRINT_CONFIG.activeColor) : hexToRgb(BLUEPRINT_CONFIG.lineColor);
        const r = cr[0];
        const g = cr[1];
        const b = cr[2];
        lineData[o++] = s.x;
        lineData[o++] = s.y;
        lineData[o++] = r;
        lineData[o++] = g;
        lineData[o++] = b;
        lineData[o++] = a;
        lineData[o++] = t.x;
        lineData[o++] = t.y;
        lineData[o++] = r;
        lineData[o++] = g;
        lineData[o++] = b;
        lineData[o++] = a;
        count += 2;
      }
      gl.useProgram(this.lineProgram);
      gl.uniform2f(gl.getUniformLocation(this.lineProgram, "u_center"), camera.x, camera.y);
      gl.uniform2f(gl.getUniformLocation(this.lineProgram, "u_viewport"), width, height);
      gl.uniform1f(gl.getUniformLocation(this.lineProgram, "u_k"), camera.k);
      gl.bindBuffer(gl.ARRAY_BUFFER, this.lineBuf);
      gl.bufferData(gl.ARRAY_BUFFER, lineData.subarray(0, o), gl.STREAM_DRAW);
      const aPos = gl.getAttribLocation(this.lineProgram, "a_pos");
      const aColor = gl.getAttribLocation(this.lineProgram, "a_color");
      gl.enableVertexAttribArray(aPos);
      gl.vertexAttribPointer(aPos, 2, gl.FLOAT, false, GLGalaxyRenderer.LINE_STRIDE * 4, 0);
      gl.enableVertexAttribArray(aColor);
      gl.vertexAttribPointer(aColor, 4, gl.FLOAT, false, GLGalaxyRenderer.LINE_STRIDE * 4, 8);
      if (count > 0) gl.drawArrays(gl.LINES, 0, count);
      gl.disableVertexAttribArray(aPos);
      gl.disableVertexAttribArray(aColor);
    }

    // ---- 节点
    if (this.nodeProgram && nodes.length > 0) {
      const data = new Float32Array(nodes.length * GLGalaxyRenderer.NODE_STRIDE);
      let o = 0;
      for (const n of nodes) {
        const isSel = n.id === selectedId;
        const isFocus = n.id === focusId;
        const isHov = n.id === hoveredId;
        let alpha = 1;
        if (focusId !== null && !isFocus && !neighbors.has(n.id)) alpha = 0.22;
        else if (selectedId !== null && !isSel && !neighbors.has(n.id)) alpha = 0.22;
        // 点精灵画不出"白底 + 深描边"的卡片，退而用深蓝黑实心点；
        // 选中/悬停改用类型色放大，才能和白底拉开对比。
        const rgb = hexToRgb(isSel || isFocus || isHov ? n.color : BLUEPRINT_CONFIG.nodeBorder);
        data[o++] = n.x;
        data[o++] = n.y;
        data[o++] = rgb[0];
        data[o++] = rgb[1];
        data[o++] = rgb[2];
        data[o++] = alpha;
        data[o++] = isSel || isFocus || isHov ? n.r + 3 : n.r;
      }
      gl.useProgram(this.nodeProgram);
      gl.uniform2f(gl.getUniformLocation(this.nodeProgram, "u_center"), camera.x, camera.y);
      gl.uniform2f(gl.getUniformLocation(this.nodeProgram, "u_viewport"), width, height);
      gl.uniform1f(gl.getUniformLocation(this.nodeProgram, "u_k"), camera.k);
      gl.uniform1f(gl.getUniformLocation(this.nodeProgram, "u_dpr"), dpr);
      const fish = input.fisheye;
      gl.uniform4f(
        gl.getUniformLocation(this.nodeProgram, "u_fish"),
        fish && fish.on ? 1 : 0,
        fish ? fish.x : 0,
        fish ? fish.y : 0,
        fish ? fish.radius : 0
      );
      gl.uniform1f(gl.getUniformLocation(this.nodeProgram, "u_fishScale"), fish ? fish.scale : 1);
      gl.bindBuffer(gl.ARRAY_BUFFER, this.nodeBuf);
      gl.bufferData(gl.ARRAY_BUFFER, data, gl.STREAM_DRAW);
      const stride = GLGalaxyRenderer.NODE_STRIDE * 4;
      const aPos = gl.getAttribLocation(this.nodeProgram, "a_pos");
      const aColor = gl.getAttribLocation(this.nodeProgram, "a_color");
      const aSize = gl.getAttribLocation(this.nodeProgram, "a_size");
      gl.enableVertexAttribArray(aPos);
      gl.vertexAttribPointer(aPos, 2, gl.FLOAT, false, stride, 0);
      gl.enableVertexAttribArray(aColor);
      gl.vertexAttribPointer(aColor, 4, gl.FLOAT, false, stride, 8);
      gl.enableVertexAttribArray(aSize);
      gl.vertexAttribPointer(aSize, 1, gl.FLOAT, false, stride, 24);
      gl.drawArrays(gl.POINTS, 0, nodes.length);
      gl.disableVertexAttribArray(aPos);
      gl.disableVertexAttribArray(aColor);
      gl.disableVertexAttribArray(aSize);
    }

    this.lastFrameMs = performance.now() - t0;
  }

  destroy() {
    const gl = this.gl;
    if (this.nodeBuf) gl.deleteBuffer(this.nodeBuf);
    if (this.lineBuf) gl.deleteBuffer(this.lineBuf);
    if (this.nodeProgram) gl.deleteProgram(this.nodeProgram);
    if (this.lineProgram) gl.deleteProgram(this.lineProgram);
    this.nodeBuf = null;
    this.lineBuf = null;
    this.nodeProgram = null;
    this.lineProgram = null;
    // 主动归还上下文：Chromium 系同时存活的 WebGL 上下文有上限，
    // 反复进出图谱页而不释放，最终会让新建上下文直接失败。
    try {
      const lose = gl.getExtension("WEBGL_lose_context");
      lose?.loseContext();
    } catch {
      /* 忽略 */
    }
  }
}

/** #rrggbb -> [r,g,b]（0~1）。 */
function hexToRgb(hex: string): [number, number, number] {
  const s = hex.trim();
  if (s.length === 7 && s[0] === "#") {
    return [
      parseInt(s.slice(1, 3), 16) / 255,
      parseInt(s.slice(3, 5), 16) / 255,
      parseInt(s.slice(5, 7), 16) / 255,
    ];
  }
  return [0.6, 0.6, 0.6];
}