import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";

type ErrorBoundaryState = {
  error: string | null;
};

class ErrorBoundary extends React.Component<React.PropsWithChildren, ErrorBoundaryState> {
  state: ErrorBoundaryState = {
    error: null,
  };

  static getDerivedStateFromError(error: Error): ErrorBoundaryState {
    return {
      error: error.message || "发生了未处理的前端错误。",
    };
  }

  componentDidCatch(error: Error) {
    console.error("FileSync Notes UI crashed:", error);
  }

  render() {
    if (this.state.error) {
      return (
        <main
          style={{
            minHeight: "100vh",
            display: "grid",
            placeItems: "center",
            padding: "24px",
            background: "#f7f9f8",
            fontFamily: "\"Segoe UI Variable\", \"Segoe UI\", system-ui, sans-serif",
          }}
        >
          <section
            style={{
              width: "min(720px, 100%)",
              padding: "28px",
              borderRadius: "24px",
              background: "white",
              boxShadow: "0 18px 40px rgba(29, 57, 61, 0.08)",
              border: "1px solid rgba(11, 31, 34, 0.08)",
            }}
          >
            <h1 style={{ marginTop: 0 }}>界面遇到了一个错误</h1>
            <p>应用没有正常渲染，下面是捕获到的错误信息：</p>
            <pre
              style={{
                padding: "16px",
                borderRadius: "16px",
                background: "#f2f6f5",
                whiteSpace: "pre-wrap",
                wordBreak: "break-word",
              }}
            >
              {this.state.error}
            </pre>
          </section>
        </main>
      );
    }

    return this.props.children;
  }
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <ErrorBoundary>
      <App />
    </ErrorBoundary>
  </React.StrictMode>
);
