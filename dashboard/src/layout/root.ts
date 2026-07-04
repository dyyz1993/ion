/**
 * 三栏根布局（Yoga flexbox 自动响应式）
 */
import { Box, Text } from "@opentui/core";
import { state } from "../state";
import { COLORS } from "../theme";
import { renderTree } from "./tree";
import { renderKanban } from "./kanban";
import { renderDetail } from "./detail";
import { renderInputBar } from "./input_bar";
import { renderStatusBar } from "./status_bar";
import { renderCreateModal } from "./create_modal";
import { renderTodoPanel } from "./todo_panel";
import { renderMemPanel } from "./mem_panel";

export function renderRoot(renderer: any): any {
  // 创建模态打开时，叠加在最上层
  if (state.createModal) {
    return Box(
      { flexDirection: "column", width: "100%", height: "100%", bg: COLORS.bg },
      [renderCreateModal(renderer)]
    );
  }

  // Focus 模式：详情占大部分 + 侧栏
  if (state.focusMode && state.selectedSessionId) {
    return Box(
      {
        flexDirection: "column",
        width: "100%",
        height: "100%",
        bg: COLORS.bg,
      },
      [
        Box(
          { flexDirection: "row", flexGrow: 1, width: "100%" },
          [
            // 主区域（详情 + 输入框）
            Box(
              { flexDirection: "column", flexGrow: 4, width: "100%" },
              [
                Box({ flexGrow: 1 }, [renderDetail(renderer, true)]),
                Box({ height: 3 }, [renderInputBar(renderer)]),
              ]
            ),
            // 侧栏
            Box({ flexDirection: "column", flexGrow: 1 }, [
              renderTodoPanel(renderer),
              renderMemPanel(renderer),
            ]),
          ]
        ),
        renderStatusBar(renderer),
      ]
    );
  }

  // 普通模式：三栏 + 底部输入框 + 状态栏
  return Box(
    {
      flexDirection: "column",
      width: "100%",
      height: "100%",
      bg: COLORS.bg,
    },
    [
      Box(
        { flexDirection: "row", flexGrow: 1, width: "100%" },
        [
          // 左：项目树（flex 1）
          Box({ flexGrow: 1, minWidth: 20 }, [renderTree(renderer)]),
          // 中：看板（flex 3）
          Box({ flexGrow: 3 }, [renderKanban(renderer)]),
          // 右：详情（flex 1）
          Box({ flexGrow: 1, minWidth: 25 }, [renderDetail(renderer, false)]),
        ]
      ),
      // 底部输入框（3 行）
      Box({ height: 3, width: "100%" }, [renderInputBar(renderer)]),
      // 状态栏（1 行）
      renderStatusBar(renderer),
    ]
  );
}
