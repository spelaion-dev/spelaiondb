import ReactDOM from "react-dom/client";
import "@glideapps/glide-data-grid/dist/index.css";
import "./styles.css";
import App from "./App";

// No StrictMode: automatic mode must not run twice.
ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(<App />);
