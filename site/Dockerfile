FROM nginx:alpine
COPY nginx.conf /etc/nginx/conf.d/default.conf
COPY index.html /usr/share/nginx/html/index.html
COPY nemesis-8.png /usr/share/nginx/html/nemesis-8.png
EXPOSE 8080
CMD ["nginx", "-g", "daemon off;"]
