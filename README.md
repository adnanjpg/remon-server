# Remon

## Environment
1. create a copy of the `.env.example` file in the root directory of the project, and name it `.env`
2. follow the instructions in the [Firebase Documentation](https://firebase.google.com/docs/cloud-messaging/auth-server#provide-credentials-manually) to create a service account. after you create a service account, and download the json file
3. set the value of `GOOGLE_APPLICATION_CREDENTIALS` in the `.env` file to the path of the json file you downloaded, as shown in the `.env.example` file
   