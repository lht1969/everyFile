interface StatusBarProps {
  message: string;
  indexStatus: {
    status: string;
    file_count: number;
    progress: number;
    message: string;
    volumes: string[];
    last_update: string;
  };
  isAdmin: boolean;
}

function StatusBar({ message, indexStatus, isAdmin }: StatusBarProps) {
  return (
    <div className="status-bar">
      <div className="status-left">
        <span className="status-message">{message}</span>
      </div>
      <div className="status-right">
        {indexStatus.volumes.length > 0 && (
          <span className="volume-info">
            卷: {indexStatus.volumes.join(', ')}
          </span>
        )}
        <span className="index-status">
          {indexStatus.status === 'scanning' && indexStatus.progress < 1 ? (
            <>索引中... {Math.round(indexStatus.progress * 100)}%</>
          ) : (
            <>已索引 {indexStatus.file_count.toLocaleString()} 个文件</>
          )}
        </span>
        {indexStatus.last_update && (
          <span className="last-update">
            更新: {indexStatus.last_update}
          </span>
        )}
        <span className={`admin-badge ${isAdmin ? 'admin' : 'normal'}`}>
          {isAdmin ? '管理员' : '普通用户'}
        </span>
      </div>
    </div>
  );
}

export default StatusBar;
