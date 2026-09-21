import { invoke as tauriInvoke } from "@tauri-apps/api/core";

export const isNativeRuntime = "__TAURI_INTERNALS__" in window;
export const isWebDebugRuntime = !isNativeRuntime;

const now = Math.floor(Date.now() / 1000);
let archiveProgress: Record<string, unknown> = {
  status: "idle", source: "idle", pages: 0, fetched: 0, saved: 0,
  selfPages: 0, selfFetched: 0, selfSaved: 0, storedSelf: 569, storedTotal: 6186, skipped: 0,
  legacyRequests: 0, legacyFetched: 0, legacySaved: 0, legacyOffset: 0, legacyMaxOffset: 0,
  message: "网页版调试模式：不会读取真实 QQ 或本地数据库",
};
let transferProgress: Record<string, unknown> = {
  status: "idle", kind: "idle", total: 0, completed: 0, downloaded: 0,
  skipped: 0, unavailable: 0, failed: 0, current: "", message: "网页版只演示交互，不会写入文件",
};

const demoItems = [
  {
    id: 9001, cellId: "web-debug-1", publishedAt: now - 86400 * 30,
    content: "这是网页版调试数据。桌面版登录后会显示真实归档。", authorUin: "100000001",
    authorName: "调试账号", pictureUrls: ["/demo.svg"], videoUrls: [],
    likeCount: 3, commentCount: 1, likes: [{ uin: "100000002", nickname: "好友" }],
    comments: [{ uin: "100000002", nickname: "好友", content: "界面调试正常", createdAt: now - 86400 * 29, replies: [] }],
  },
  {
    id: 9002, cellId: "web-debug-2", publishedAt: now - 86400 * 400,
    content: "可在这里调试时间范围、PDF、ZIP 和媒体下载按钮。", authorUin: "100000001",
    authorName: "调试账号", pictureUrls: [], videoUrls: [], likeCount: 0, commentCount: 0, likes: [], comments: [],
  },
];

function finishArchiveMock() {
  window.setTimeout(() => {
    archiveProgress = {
      status: "completed", source: "idle", pages: 12, fetched: 186, saved: 82,
      selfPages: 4, selfFetched: 90, selfSaved: 68, storedSelf: 637, storedTotal: 6268, skipped: 0,
      legacyRequests: 0, legacyFetched: 0, legacySaved: 0, legacyOffset: 0, legacyMaxOffset: 0,
      message: "网页模拟完成：本人说说与互动流已合并去重（未访问真实数据）",
    };
  }, 1400);
}

function finishTransferMock(kind: string) {
  window.setTimeout(() => {
    transferProgress = {
      status: "completed", kind, total: 24, completed: 24, downloaded: 20,
      skipped: 4, unavailable: 2, failed: 0, current: "", outputDir: "D:\\QQ空间归档（网页模拟）",
      message: "网页模拟完成：桌面版才会真正生成文件",
    };
  }, 1200);
}

async function webDebugInvoke<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  switch (command) {
    case "get_archive_progress": return archiveProgress as T;
    case "start_feed_archive":
      archiveProgress = { ...archiveProgress, status: "running", source: "self", message: "网页模拟：正在合并本人说说与互动流…" };
      finishArchiveMock();
      await new Promise((resolve) => window.setTimeout(resolve, 1500));
      return archiveProgress as T;
    case "recommend_legacy_max_offset": {
      const year = Number(args?.targetYear || new Date().getFullYear());
      const recommendations: Record<number, number> = { 2023: 2500, 2022: 3500, 2021: 5000, 2020: 8000, 2019: 12000, 2018: 18000, 2017: 25000, 2016: 35000, 2015: 50000, 2014: 80000, 2013: 90000, 2012: 100000, 2011: 110000, 2010: 120000, 2009: 130000 };
      return (year >= 2024 ? 1500 : recommendations[year] || 150000) as T;
    }
    case "start_legacy_history_scan": {
      const maxOffset = Number(args?.maxOffset || 50000);
      archiveProgress = { ...archiveProgress, status: "running", source: "legacy", legacyRequests: 18, legacyFetched: 436, legacySaved: 120, legacyOffset: Math.min(1800, maxOffset), legacyMaxOffset: maxOffset, legacyTargetYear: Number(args?.targetYear || 2015), legacyEarliestAt: Math.floor(new Date("2018-01-02").getTime() / 1000), message: "网页模拟：正在扫描网页版历史 offset 1800…" };
      window.setTimeout(() => { archiveProgress = { ...archiveProgress, legacyRequests: 36, legacyFetched: 802, legacySaved: 226, legacyOffset: Math.min(3600, maxOffset), message: "网页模拟：历史深扫实时进度更新中…" }; }, 700);
      window.setTimeout(() => { archiveProgress = { ...archiveProgress, status: "completed", source: "legacy", legacyRequests: 50, legacyFetched: 1080, legacySaved: 302, legacyOffset: maxOffset, legacyMaxOffset: maxOffset, legacyEarliestAt: Math.floor(new Date("2015-03-04").getTime() / 1000), message: "网页模拟完成：历史深扫已合并去重（未访问真实 QQ）" }; }, 1500);
      await new Promise((resolve) => window.setTimeout(resolve, 1600));
      return archiveProgress as T;
    }
    case "cancel_feed_archive": archiveProgress = { ...archiveProgress, status: "cancelled", source: "idle", message: "网页模拟任务已停止" }; return undefined as T;
    case "list_archive_skips": return [] as T;
    case "clear_resolved_archive_skips": return 0 as T;
    case "retry_all_archive_skips": return { total: 0, recovered: 0, failed: 0, recoveredRecords: 0 } as T;
    case "list_archived_feeds": return demoItems as T;
    case "count_archived_feeds": return demoItems.length as T;
    case "get_archived_feed": return (demoItems.find((item) => item.id === Number(args?.id)) || demoItems[0]) as T;
    case "list_archived_media": return { items: [], total: 0, years: [] } as T;
    case "export_archived_html": return "<!doctype html><meta charset=utf-8><h1>网页版调试导出</h1>" as T;
    case "get_archive_storage_info": return {
      rootDir: "D:\\QQ空间归档（网页模拟）", databasePath: "D:\\QQ空间归档（网页模拟）\\qzone-archive.sqlite3",
      imagesDir: "D:\\QQ空间归档（网页模拟）\\images", videosDir: "D:\\QQ空间归档（网页模拟）\\videos",
      exportsDir: "D:\\QQ空间归档（网页模拟）\\exports", custom: true,
    } as T;
    case "set_archive_storage_dir": return webDebugInvoke<T>("get_archive_storage_info");
    case "get_transfer_progress": return transferProgress as T;
    case "start_media_download":
      transferProgress = { ...transferProgress, status: "running", kind: "media", message: "网页模拟：正在下载媒体…" };
      finishTransferMock("media");
      await new Promise((resolve) => window.setTimeout(resolve, 1300));
      return transferProgress as T;
    case "cancel_transfer": transferProgress = { ...transferProgress, status: "cancelled", message: "网页模拟任务已停止" }; return undefined as T;
    case "export_archive_pdf":
      transferProgress = { ...transferProgress, status: "running", kind: "pdf", total: 3, completed: 1, message: "网页模拟：正在生成分年份 PDF…" };
      finishTransferMock("pdf");
      await new Promise((resolve) => window.setTimeout(resolve, 1300));
      return { path: "D:\\QQ空间归档（网页模拟）", files: ["QQ空间归档-2026.pdf"], warnings: [], dynamics: 2, downloaded: 1, skipped: 0, unavailable: 0, failed: 0 } as T;
    case "export_share_zip":
      transferProgress = { ...transferProgress, status: "running", kind: "zip", total: 24, completed: 5, message: "网页模拟：正在下载媒体并压缩 ZIP…" };
      finishTransferMock("zip");
      await new Promise((resolve) => window.setTimeout(resolve, 1300));
      return { path: String(args?.outputPath || "D:\\QQ空间归档.zip"), files: ["index.html"], warnings: [], dynamics: 2, downloaded: 1, skipped: 0, unavailable: 0, failed: 0 } as T;
    case "get_archive_overview": return { dynamics: 2, pictures: 1, comments: 1, likes: 3, databaseBytes: 4096 } as T;
    case "list_interactors": return [] as T;
    case "get_interaction_ranking": return [] as T;
    case "delete_archived_feeds": return 0 as T;
    case "clear_archived_feeds": return 0 as T;
    case "delete_all_app_data": return undefined as T;
    default: throw new Error(`网页版调试模式未模拟命令：${command}`);
  }
}

export function invokeApp<T>(command: string, args?: Record<string, unknown>): Promise<T> {
  return isNativeRuntime ? tauriInvoke<T>(command, args) : webDebugInvoke<T>(command, args);
}
