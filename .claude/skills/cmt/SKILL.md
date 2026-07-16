---
name: cmt
description: 自动分析 git diff 并生成符合规范的 commit message
---

# /cmt — 自动生成 git commit message

根据当前 git diff（优先 staged，fallback unstaged）分析变更内容，按项目规范自动生成 commit message。

## 工作流程

1. **收集变更信息**
   - 优先检查 `git diff --cached`（staged changes）
   - 若无 staged 内容，fallback 到 `git diff`（unstaged changes）
   - 同时收集 `git status --short` 了解整体情况（新增/修改/删除的文件清单）
   - 若均无变更，报错退出

2. **分析变更**
   - 识别变更类型：`feat` / `fix` / `refactor` / `docs` / `chore` / `cleanup` / `test`
   - 识别影响范围（哪个模块、哪个功能）
   - 总结变更内容的本质（WHY 而不是 WHAT）

3. **生成 commit message**

   - 标题行：`<type>: <简短描述>`（英文，50 字以内）
   - 正文：说明 WHY 而不是 WHAT（英文，每行 72 字以内）

4. **输出 commit message**

   用以下格式展示给用户，方便复制或用管道提交：
   ```
   <type>: <summary>

   <body>
   ```

5. **可选交互**

   - 如果用户确认（回答 y/yes），直接执行 `git commit`
   - 如果用户要修改，根据反馈调整
   - 如果用户拒绝，直接退出

## Type 选择规则（按优先级）

| 变更内容 | type |
|---------|------|
| 新功能、新增模块、新增文件 | `feat` |
| Bug 修复、行为修正 | `fix` |
| 代码重构、Extract 函数、Trait 提取 | `refactor` |
| 测试增补或修改 | `test` |
| 文档、注释、CLAUDE.md | `docs` |
| 编译 warning 清除、dead code 标记、lint 修复 | `chore` |
| 依赖变更（Cargo.toml / Cargo.lock） | `chore` |
| CI 配置、构建脚本 | `chore` |
| 重命名、移动文件 | `cleanup` |

## 注意事项

- 标题行永远用**中文**
- 正文永远用**中文**
- 正文永远是 WHY 而不是 WHAT——WHAT 看 diff 就知道
- 如果用户有多个逻辑上独立的变更，提示用户分开提交
- 生成的 message 要展示给用户确认后再提交
