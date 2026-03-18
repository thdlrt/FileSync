# FileSync Notes

一个面向 Windows 的轻量桌面同步工具，用来把统一笔记目录中的文档，单向同步到项目里的 `README`、教程文档或文档目录。

适合这样的工作流：

- 笔记统一维护在 Obsidian 或普通文件夹里
- 项目仓库里只保留最终需要提交的文档副本
- 修改源笔记后，希望目标文件自动同步，避免手动复制

## 功能概览

- 单向同步规则
  - `文件 -> 文件`
  - `文件夹 -> 文件夹`
- 源路径必须存在，目标路径允许不存在
- 首次同步自动创建目标目录和目标文件
- 自动同步
  - 文件监听
  - 轮询兜底
- 手动同步
  - 单条规则立即同步
  - 全部规则立即同步
- 备份机制
  - 仅在覆盖已有目标文件前创建备份
  - 备份写入目标同级 `.back` 目录
  - 支持按保留天数自动清理
- 托盘常驻
  - 开机自启
  - 关闭窗口后隐藏到托盘
- 调试辅助
  - 同步历史
  - 运行日志

## 技术栈

- 前端：React 19 + TypeScript + Vite
- 桌面端：Tauri 2
- 后端：Rust

## 目录结构

```text
E:\code\other\FileSync
├─ public/                  静态资源
├─ src/                     React 前端源码
├─ src-tauri/               Tauri / Rust 桌面端源码
│  ├─ icons/                安装包和托盘图标资源
│  ├─ src/                  Rust 业务逻辑
│  └─ tauri.conf.json       Tauri 配置
├─ package.json             前端与 Tauri 脚本
├─ README.md
└─ .gitignore
```

说明：

- `dist/` 是前端构建产物，不需要提交到 GitHub
- `src-tauri/target/` 是 Rust/Tauri 构建产物，不需要提交到 GitHub
- `dist-portable/` 或其他本地测试目录也不需要提交

## 开发

安装依赖：

```powershell
npm install
```

启动开发环境：

```powershell
npm run tauri:dev
```

仅构建前端：

```powershell
npm run build
```

## 打包

调试构建：

```powershell
npm run tauri:build:debug
```

Release 安装包：

```powershell
npm run tauri:build:release
```

默认输出位置：

- 调试可执行文件：
  - `src-tauri\target\debug\filesync-notes.exe`
- Release 可执行文件：
  - `src-tauri\target\release\filesync-notes.exe`
- Release 安装包：
  - `src-tauri\target\release\bundle\nsis\`
  - `src-tauri\target\release\bundle\msi\`

## 使用说明

### 文件规则

适用于：

- 单个 Markdown 文档同步到项目中的单个目标文件

示例：

- 源：`E:\notes\UE\README.md`
- 目标：`E:\project\README.md`

注意：

- 目标路径是“目标文件完整路径”
- 如果你先选了目标目录，程序会自动补上源文件名

### 文件夹规则

适用于：

- 一整个文档目录同步到项目文档目录

示例：

- 源：`E:\notes\docs`
- 目标：`E:\project\docs`

### 备份

- 覆盖已有目标文件前，会在目标同级 `.back` 目录生成备份
- 如果本次目标内容与源内容一致，则不会覆盖，也不会生成备份

## GitHub 提交建议

建议提交这些内容：

- `src/`
- `src-tauri/`
- `public/`
- `package.json`
- `package-lock.json`
- `README.md`
- `.gitignore`

建议不要提交这些内容：

- `node_modules/`
- `dist/`
- `dist-portable/`
- `src-tauri/target/`

## 当前版本

- `0.1.0`

