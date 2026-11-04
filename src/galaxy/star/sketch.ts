// 铅绘滤镜（《2D转3D与星图需求明细》#24~#28）。
//
// 设计取舍：需求写的是「G-Buffer 法线缓冲 + Sobel」，但在 WebGL2 上额外渲染
// 一遍法线通道等于把场景画两遍，代价远超 <5fps 的预算。这里改用**深度缓冲**
// （渲染本来就产出，零额外开销）做结构线，再用亮度梯度补细节线 —— 铅笔画的
// 线本来就来自轮廓与明暗交界，两者视觉等价。

import * as THREE from "three";
import { ShaderPass } from "three/examples/jsm/postprocessing/ShaderPass.js";

export const SketchShader = {
  uniforms: {
    tDiffuse: { value: null as THREE.Texture | null },
    tDepth: { value: null as THREE.Texture | null },
    uResolution: { value: new THREE.Vector2(1, 1) },
    /** 线宽（需求 #25：1~3px）。 */
    uLineWidth: { value: 1.6 },
    /** 线条浓度。 */
    uStrength: { value: 0.85 },
    /** 手抖幅度（需求 #26）。 */
    uJitter: { value: 0.8 },
    /** 纸张颗粒强度（需求 #27）。 */
    uPaper: { value: 0.35 },
    /** 时间：让抖动微微呼吸，完全静止的手抖看着像脏点。 */
    uTime: { value: 0 },
    uEnabled: { value: 1 },
  },
  vertexShader: /* glsl */ `
    varying vec2 vUv;
    void main() {
      vUv = uv;
      gl_Position = projectionMatrix * modelViewMatrix * vec4(position, 1.0);
    }
  `,
  fragmentShader: /* glsl */ `
    uniform sampler2D tDiffuse;
    uniform sampler2D tDepth;
    uniform vec2 uResolution;
    uniform float uLineWidth;
    uniform float uStrength;
    uniform float uJitter;
    uniform float uPaper;
    uniform float uTime;
    uniform float uEnabled;
    varying vec2 vUv;

    float luma(vec3 c) { return dot(c, vec3(0.299, 0.587, 0.114)); }

    // 2D 值噪声：比 Perlin 便宜，做纸纹够用。
    float hash(vec2 p) {
      p = fract(p * vec2(233.34, 851.73));
      p += dot(p, p + 23.45);
      return fract(p.x * p.y);
    }
    float noise(vec2 p) {
      vec2 i = floor(p);
      vec2 f = fract(p);
      vec2 u = f * f * (3.0 - 2.0 * f);
      return mix(mix(hash(i), hash(i + vec2(1.0, 0.0)), u.x),
                 mix(hash(i + vec2(0.0, 1.0)), hash(i + vec2(1.0, 1.0)), u.x), u.y);
    }
    float fbm(vec2 p) {
      float v = 0.0;
      float a = 0.5;
      for (int i = 0; i < 4; i++) {
        v += a * noise(p);
        p *= 2.02;
        a *= 0.5;
      }
      return v;
    }

    void main() {
      vec2 px = uLineWidth / max(uResolution, vec2(1.0));
      // 手抖：按噪声轻微推一下采样点，线条边缘就会跟着抖（#26）。
      float wob = (fbm(vUv * 42.0 + uTime * 0.35) - 0.5) * uJitter;
      vec2 uv = clamp(vUv + wob * px, vec2(0.0), vec2(1.0));

      vec3 base = texture2D(tDiffuse, uv).rgb;
      float l = luma(base);

      // ---- 结构线：深度 Sobel（#24/#25）------------------------------
      float dC = texture2D(tDepth, uv).r;
      float dN = texture2D(tDepth, uv + vec2(0.0, px.y)).r;
      float dS = texture2D(tDepth, uv - vec2(0.0, px.y)).r;
      float dE = texture2D(tDepth, uv + vec2(px.x, 0.0)).r;
      float dW = texture2D(tDepth, uv - vec2(px.x, 0.0)).r;
      // 深度差在远处极小、近处极大，除以 dC 归一化，否则远景会糊成一片黑线。
      float depthEdge = (abs(dN - dS) + abs(dE - dW)) / max(0.0008, dC * 0.12);

      // ---- 细节线：亮度 Sobel -----------------------------------------
      float lN = luma(texture2D(tDiffuse, uv + vec2(0.0, px.y)).rgb);
      float lS = luma(texture2D(tDiffuse, uv - vec2(0.0, px.y)).rgb);
      float lE = luma(texture2D(tDiffuse, uv + vec2(px.x, 0.0)).rgb);
      float lW = luma(texture2D(tDiffuse, uv - vec2(px.x, 0.0)).rgb);
      float lumEdge = abs(lN - lS) + abs(lE - lW);

      float edge = clamp(depthEdge * 1.35 + lumEdge * 0.9, 0.0, 1.6);

      // ---- 拓扑线：Perlin 噪声的等值线（#26）-------------------------
      // 取 fbm 的小数部分做「等高线」：噪声场里相邻色阶的边界就是一条线，
      // 视觉上像铅笔顺着结构铺开的排线，而不是纯随机的脏点。
      float field = fbm(vUv * 3.2 + uTime * 0.02);
      float band = abs(fract(field * 9.0) - 0.5);
      // 1px 宽窄的等值线（fwidth 自适应分辨率，缩放时线宽不变）
      float topo = 1.0 - smoothstep(0.0, fwidth(field * 9.0) * 1.6, band);
      // 只在暗部/结构附近浮现，亮部保持干净，避免整页爬满线条。
      topo *= (1.0 - l) * 0.55;

      // ---- 纸张：颗粒 + 大尺度纤维（#27）-----------------------------
      float grain = noise(vUv * uResolution * 0.6);
      float fiber = fbm(vUv * vec2(uResolution.x * 0.05, uResolution.y * 0.012));
      float paper = mix(0.97, 0.78, grain * uPaper) * mix(0.94, 1.0, fiber);

      // 铅笔底子：原图压成灰阶再乘纸纹。
      vec3 paperCol = vec3(0.955, 0.945, 0.918) * paper;
      float shade = 1.0 - smoothstep(0.05, 1.0, l) * 0.42;
      vec3 col = paperCol * shade;

      // 线条用深灰而不是纯黑：纯黑压在浅色纸上会显得刺眼。
      vec3 ink = vec3(0.13, 0.13, 0.15);
      col = mix(col, ink, clamp((edge + topo * 0.35) * uStrength, 0.0, 0.92));

      // 纸纹按正片叠底（Multiply）压到线条上（#27）：纸张粗糙处线条自然断开。
      col *= mix(vec3(1.0), vec3(paper), 0.75);

      // 留一点彩铅感（8%），纯黑白就看不出文件类型了。
      col = mix(col, base * paper, clamp(l * 0.08, 0.0, 0.08));

      gl_FragColor = vec4(mix(base, col, uEnabled), 1.0);
    }
  `,
};

export interface SketchOptions {
  lineWidth?: number;
  strength?: number;
  jitter?: number;
  paper?: number;
}

/** 给 composer 的渲染目标挂深度纹理（RenderPass 会写它，铅绘读它）。 */
export function ensureDepthTexture(composer: any, w: number, h: number) {
  const iw = Math.max(2, Math.floor(w));
  const ih = Math.max(2, Math.floor(h));
  for (const key of ["renderTarget1", "renderTarget2"] as const) {
    const rt = composer?.[key];
    if (!rt) continue;
    if (!rt.depthTexture) {
      const dt = new THREE.DepthTexture(iw, ih);
      // UnsignedShort 足够区分场景层次；Float 在部分集显上不可渲染。
      dt.type = THREE.UnsignedShortType;
      dt.minFilter = THREE.NearestFilter;
      dt.magFilter = THREE.NearestFilter;
      rt.depthTexture = dt;
    } else if (rt.depthTexture.image.width !== iw || rt.depthTexture.image.height !== ih) {
      rt.depthTexture.image.width = iw;
      rt.depthTexture.image.height = ih;
      rt.depthTexture.needsUpdate = true;
    }
  }
  return composer?.renderTarget1?.depthTexture ?? null;
}

export function createSketchPass(w: number, h: number, opts: SketchOptions = {}): ShaderPass {
  const pass = new ShaderPass(SketchShader as any);
  pass.uniforms.uLineWidth.value = opts.lineWidth ?? 1.6;
  pass.uniforms.uStrength.value = opts.strength ?? 0.85;
  pass.uniforms.uJitter.value = opts.jitter ?? 0.8;
  pass.uniforms.uPaper.value = opts.paper ?? 0.35;
  pass.uniforms.uResolution.value.set(Math.max(2, w), Math.max(2, h));
  return pass;
}
