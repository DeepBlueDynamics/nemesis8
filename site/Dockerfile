FROM nginx:alpine
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY index.html /usr/share/nginx/html/index.html
COPY nemesis-8.png /usr/share/nginx/html/nemesis-8.png
COPY install.ps1 /usr/share/nginx/html/install.ps1
COPY install.sh /usr/share/nginx/html/install.sh
COPY docs.html /usr/share/nginx/html/docs.html
EXPOSE 8080
CMD ["nginx", "-g", "daemon off;"]
