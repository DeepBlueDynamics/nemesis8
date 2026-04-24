FROM nginx:alpine
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY install.sh /usr/share/nginx/html/install.sh
COPY install.ps1 /usr/share/nginx/html/install.ps1
EXPOSE 8080
CMD ["nginx", "-g", "daemon off;"]
