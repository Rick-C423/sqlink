import { build } from "esbuild";
import { mkdtemp, realpath, readdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, relative, isAbsolute, resolve, delimiter } from "node:path";
import { spawnSync } from "node:child_process";

// 复用前端构建工具编译测试，使用 Node 自带测试运行器，不需要浏览器或数据库服务。
const parent = await realpath(tmpdir());
const directory = await mkdtemp(join(parent, "sqlink-tests-"));
const entries = (await readdir("tests")).filter((name) => /\.test\.tsx?$/.test(name)).sort().map((name) => `tests/${name}`);
try {
  await build({
    entryPoints: entries,
    outdir: directory,
    bundle: true,
    // React 的异步 act 依赖真实 Node module.require；打进 bundle 会退到浏览器消息通道且不退出。
    external: ["react", "react/*", "react-dom", "react-dom/*", "react-test-renderer", "react-test-renderer/*"],
    platform: "node",
    format: "cjs",
    outExtension: { ".js": ".cjs" },
    sourcemap: "inline",
    tsconfig: "tsconfig.json",
    logLevel: "warning",
  });
  const result = spawnSync(process.execPath, ["--test", ...entries.map((entry) =>
    join(directory, entry.split("/").at(-1).replace(/\.tsx?$/, ".cjs"))
  )], {
    stdio: "inherit",
    env: { ...process.env, NODE_PATH: [resolve("node_modules"), process.env.NODE_PATH].filter(Boolean).join(delimiter) },
    timeout: 120_000,
  });
  if (result.error) throw result.error;
  process.exitCode = result.status ?? 1;
} finally {
  const child = relative(parent, directory);
  if (!isAbsolute(child) && child.startsWith("sqlink-tests-") && !child.includes("..") && !/[\\/]/.test(child)) {
    await rm(directory, { recursive: true, force: true });
  }
}
