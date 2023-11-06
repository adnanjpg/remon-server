# Remon

**Disclaimer: Early Development Stage •
Please note that this application is currently in the early development stages and may not be stable for production use.**



## Overview
Remon is an open-source, **blazingly fast 😳🚀** application that allows you to monitor your server's status in real-time and receive notifications when critical events occur. This project consists of two main components: a server application that runs on your server and a mobile app that you can install on your smartphone.

The server application constantly monitors various server parameters such as CPU usage, RAM usage, storage availability, and more. The server application is built with performance in mind, so it uses minimal resources and has a negligible impact on your server's performance.

The mobile app provides a user-friendly interface for configuring what events trigger notifications and viewing real-time server status through live graphs. The mobile app is designed for both Android and iOS platforms, ensuring accessibility to a wide range of users.

## Features
1. Real-time Monitoring: The server application continuously collects server data and updates the mobile app in real-time.

2. Configurable Notifications: Users can set thresholds for various server metrics (e.g., RAM usage > 63%) to receive push notifications when those thresholds are exceeded.

3. Live Graphs: The mobile app provides interactive graphs that visualize server performance over time, making it easy to spot trends and anomalies.

4. Multi-Platform: The mobile app is designed for both Android and iOS platforms, ensuring accessibility to a wide range of users.

5. Open Source: This project is open source, so you can customize and extend it to meet your specific needs.

## Setup
1. create a copy of the `.env.example` file in the root directory of the project, and name it `.env`
2. follow the instructions in the [Firebase Documentation](https://firebase.google.com/docs/cloud-messaging/auth-server#provide-credentials-manually) to create a service account. after you create a service account, and download the json file
3. set the value of `GOOGLE_APPLICATION_CREDENTIALS` in the `.env` file to the path of the json file you downloaded, as shown in the `.env.example` file
   
