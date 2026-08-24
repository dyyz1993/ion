# 会话工作空间 UI 自动验收（Chromium + 真实 LLM）

用法（host + 网关就绪后）：
  node tools/accept_workspace_ui.mjs

验收点：LIVE 模式 → 发真实提示词 → 事件 workspace_session_created 到达 → 卡片渲染 →
点击跳转子会话 → 子会话聊天页加载。截图输出 /tmp/ion_ws_accept_*.png。
依赖：全局 node_modules 的 playwright + /Applications/Chromium.app。
