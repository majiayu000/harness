(function () {
  window.HC.X = {
    usage: { tokens: '—', cost: '—', turns: 0, cache: '—', hourly: [], quotas: [], byProject: [], byAgent: [], byModel: [] },
    skills: [], rules: [], evals: [], memory: {}, intake: [],
    reconcile: { last: '—', rows: [] },
    gc: { last: '—', next: '—', signals: 0 },
    hostProjects: {},
  };
})();
