import { observe } from "./common.js";

export default {
  id: "conversations.identity.v1",
  async tui(api, options) {
    api.lifecycle.onDispose(observe(
      () => {
        const route = api.route.current;
        return route.name === "session" ? route.params.sessionID : undefined;
      },
      id => api.state.session.get(id),
      options,
    ));
  },
};
