interface StatusBarProps {
  message: string;
  indexStatus: {
    status: string;
    file_count: number;
    progress: number;
    message: string;
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
        <span className="index-status">
          {indexStatus.status === 'scanning' && indexStatus.progress < 1 ? (
            <>索引中... {Math.round(indexStatus.progress * 100)}%</>
          ) : (
            <>已索引 {indexStatus.file_count} 个文件</>
          )}
        </span>
        <span className={`admin-badge ${isAdmin ? 'admin' : 'normal'}`}>
          {isAdmin ? '管理员' : '普通用户'}
        </span>
      </div>
    </div>
  );
}

export default StatusBar;