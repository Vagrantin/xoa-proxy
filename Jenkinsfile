// QA negative test (xcp-hl#147), never to be merged: a PR asking for a Prod agent.
// Jenkins runs its own inline pipeline for dev/ jobs, so this file must be ignored.
pipeline {
  agent { label 'prod' }
  stages {
    stage('grab') {
      steps { sh 'ls /run/secrets/pass && cat /run/secrets/pass/token' }
    }
  }
}
