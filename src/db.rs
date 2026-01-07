use surrealdb::engine::local::Db;
use surrealdb::Surreal;
use std::sync::Arc;

pub type SharedDb = Arc<Surreal<Db>>;

pub struct DbManager {
    pub db: SharedDb,
}

impl DbManager {
    pub async fn init() -> Result<Self, surrealdb::Error> {
        // 创建存储路径（例如当前目录下的 atlas.db 文件夹）
        let db = Surreal::new::<surrealdb::engine::local::RocksDb>("atlas.db").await?;
        
        // 选择命名空间和数据库名
        db.use_ns("atlas_ns").use_db("atlas_db").await?;
        
        // 自动创建表结构（SurrealDB 是无模式的，但你可以定义模式）
        db.query("DEFINE TABLE sessions SCHEMAFULL;").await?;
        
        Ok(Self { db: Arc::new(db) })
    }
}

/*
3. 处理“数据变大”的策略：索引与分区

当数据规模扩大时，查询速度会变慢。SurrealDB 提供了几种应对方案：

    索引（Index）：在 App 启动时确保关键字段（如 session_id, timestamp）已建立索引。

    字段投影：在 render 或 update 需要数据时，只查询需要的字段，而不是 SELECT *。

    分页加载：利用 SurrealDB 强大的 LIMIT 和 START 语法，只加载当前屏幕显示的 Session 数据。

4. 解决同步组件与异步 DB 的冲突

由于你的 Component::update 是同步的，你无法在里面直接 .await 数据库查询。

推荐方案：影子状态 (Shadow State)

    后台任务：在 tokio::spawn 中异步读取数据库。

    消息传递：查询完成后，通过 mpsc 发送给组件。

    同步更新：组件在同步的 update 中 try_recv 消息并更新 UI 缓存。

5. 配置文件与数据库的联动

既然引入了 HashMap 的动态配置，你可以将这些配置持久化到 SurrealDB 中。

    Config -> DB：将 config.toml 作为初始配置。

    运行时修改：用户在 TUI 界面修改了设置后，不仅更新 Arc<RwLock<Config>>，同时触发一条异步命令更新数据库中的 settings 表。

6. 准备清单 (Checklist)

    [ ] 安装 RocksDb 依赖：在 Linux 上可能需要安装 librocksdb-dev 或在 Windows 上配置 LLVM，因为嵌入式模式需要编译存储引擎。

    [ ] 定义模型实体：利用 serde 为你的数据实体（如 SessionRecord）派生 Serialize 和 Deserialize。

    [ ] 错误处理：由于数据库操作可能失败，建议定义一个 GlobalEvent::Error(String)，当数据库出问题时，能通过广播在 TUI 的状态栏显示警告。
*/