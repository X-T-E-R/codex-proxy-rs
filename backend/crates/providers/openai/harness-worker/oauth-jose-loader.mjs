// 提取出的固定上游模块位于临时目录；从受管 worker 的锁定依赖解析官方 jose。
export async function resolve(specifier, context, nextResolve) {
  if (specifier === "jose") {
    return nextResolve(specifier, {
      ...context,
      parentURL: new URL("./oauth-run.mjs", import.meta.url).href,
    });
  }
  return nextResolve(specifier, context);
}
