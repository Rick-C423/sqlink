import { build } from "esbuild";
import { mkdtemp, realpath, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, relative, isAbsolute } from "node:path";
import { spawnSync } from "node:child_process";

// 复用前端构建工具编译测试，使用 Node 自带测试运行器，不需要浏览器或数据库服务。
const parent = await realpath(tmpdir());
const directory = await mkdtemp(join(parent, "sqlink-tests-"));
const entries = ["sqlTable", "queryActions"].map((name) => `tests/${name}.test.ts`);
try {
  await build({
    entryPoints: entries,
    outdir: directory,
    bundle: true,
    platform: "node",
    format: "cjs",
    outExtension: { ".js": ".cjs" },
    sourcemap: "inline",
    tsconfig: "tsconfig.json",
    logLevel: "warning",
  });
  const result = spawnSync(process.execPath, ["--test", ...entries.map((entry) =>
    join(directory, entry.split("/").at(-1).replace(/\.ts$/, ".cjs"))
  )], { stdio: "inherit" });
  if (result.error) throw result.error;
  process.exitCode = result.status ?? 1;
} finally {
  const child = relative(parent, directory);
  if (!isAbsolute(child) && child.startsWith("sqlink-tests-") && !child.includes("..") && !/[\\/]/.test(child)) {
    await rm(directory, { recursive: true, force: true });
  }
}
