/**
 * 全局 state —— 简单 mutable 对象，不用 Redux
 *
 * 改 state 后调 rerender() 重建整树。
 * Input 的值也存这里（drafts），避免 VNode 重建后丢值。
 */

export type Worker = {
  worker_id: string;
  session_id: string;
  project: string;
  status: "idle" | "busy" | "stale" | "dead" | string;
  model: string;
  model_size?: string | null;
  agent: string;
  channels: string[];
  parent: string | null;
  children: string[];
  latest_output?: string[];
  log_short?: string | null;
  started_at?: number;
};

export type Project = {
  name: string;
  path: string;
  worker_count: number;
};

export type CreateModal = {
  field: "path" | "agent";
  path: string;
  agent: string;
  error?: string;
};

export type Panel = "tree" | "kanban" | "detail" | "input";

export const state = {
  // 数据
  workers: [] as Worker[],
  projects: [] as Project[],
  totalStale: 0,

  // UI 状态
  selectedSessionId: null as string | null,
  focusMode: false,
  collapsed: new Set<string>(),
  drafts: new Map<string, string>(),
  createModal: null as CreateModal | null,
  focusedPanel: "tree" as Panel,

  // 输入框（普通模式底部 + Focus 模式内）
  inputValue: "",
  inputCursor: 0,

  // 连接
  connected: false,

  // 日志（显示在右侧详情栏底部）
  logs: [] as string[],

  // 每个 session 的消息历史（实时流式 + 历史加载）
  messages: new Map<string, ChatMessage[]>(),
  // 当前 streaming 的 session（agent_start 后到 agent_end 前）
  streamingSession: null as string | null,
};

export type ChatMessage = {
  role: "user" | "assistant" | "system";
  content: string;
  streaming?: boolean;  // assistant 消息是否还在流式接收
};

/** 加一条日志（最多保留 50 条） */
export function log(msg: string) {
  const ts = new Date().toLocaleTimeString();
  state.logs.unshift(`[${ts}] ${msg}`);
  if (state.logs.length > 50) state.logs.length = 50;
}

/** rerender hook —— 由 index.ts 注册 */
let rerenderFn: (() => void) | null = null;

export function setRerenderFn(fn: () => void) {
  rerenderFn = fn;
}

export function rerender() {
  if (rerenderFn) rerenderFn();
}

/** 切换 session，保存/恢复草稿 */
export function switchSession(newSid: string | null) {
  // 保存当前草稿
  if (state.selectedSessionId && state.inputValue) {
    state.drafts.set(state.selectedSessionId, state.inputValue);
  }
  state.selectedSessionId = newSid;
  // 恢复新草稿
  state.inputValue = newSid ? state.drafts.get(newSid) ?? "" : "";
  state.inputCursor = state.inputValue.length;
}
