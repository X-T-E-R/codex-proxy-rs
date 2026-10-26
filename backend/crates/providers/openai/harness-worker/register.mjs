import { register } from "node:module";

// 在入口模块解析任何上游 TypeScript 之前安装解析钩子
register(new URL("./resolver.mjs", import.meta.url));
